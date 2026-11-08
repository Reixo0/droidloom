//! Android GNSS AIDL service for Droidloom: the host's location as Android's
//! GPS.
//!
//! While Android's location provider runs a session, the service asks the
//! host user's location broker (`droidloom-location`, through the socket the
//! supervisor projects at `/dev/socket/droidloom/location`) for fixes and
//! reports each as a GNSS location. The host decides where they come from
//! and whether Android gets any. There are no satellites, measurements or
//! assistance data to report; those extensions are unsupported.

use std::ffi::CStr;
use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use android_hardware_gnss::aidl::android::hardware::gnss::{
    ElapsedRealtime::{self as elapsed, ElapsedRealtime},
    GnssLocation::{self as location, GnssLocation},
    GnssSignalType::GnssSignalType,
    IAGnss::IAGnss,
    IAGnssRil::IAGnssRil,
    IGnss::{
        BnGnss, GnssAidingData::GnssAidingData, IGnss, PositionModeOptions::PositionModeOptions,
    },
    IGnssAntennaInfo::IGnssAntennaInfo,
    IGnssBatching::IGnssBatching,
    IGnssCallback::{
        self as callback, GnssStatusValue::GnssStatusValue, GnssSystemInfo::GnssSystemInfo,
        IGnssCallback,
    },
    IGnssConfiguration::IGnssConfiguration,
    IGnssDebug::IGnssDebug,
    IGnssGeofence::IGnssGeofence,
    IGnssMeasurementInterface::IGnssMeasurementInterface,
    IGnssNavigationMessageInterface::IGnssNavigationMessageInterface,
    IGnssPowerIndication::IGnssPowerIndication,
    IGnssPsds::IGnssPsds,
    gnss_assistance::IGnssAssistanceInterface::IGnssAssistanceInterface,
    measurement_corrections::IMeasurementCorrectionsInterface::IMeasurementCorrectionsInterface,
    visibility_control::IGnssVisibilityControl::IGnssVisibilityControl,
};
use binder::{BinderFeatures, ExceptionCode, Interface, ProcessState, Status, Strong};
use droidloom_denial_ipc::SeqPacket;
use droidloom_location_protocol::{HAS_ALTITUDE, HAS_BEARING, HAS_SPEED, RECORD_BYTES, Record};

const SERVICE_NAME: &str = "android.hardware.gnss.IGnss/default";
const BROKER: &str = "/dev/socket/droidloom/location";
/// Android asks for 1 s unless an app wants otherwise.
const DEFAULT_INTERVAL_MS: u32 = 1000;

struct State {
    callback: Option<Strong<dyn IGnssCallback>>,
    interval_ms: u32,
    active: bool,
    /// The broker connection while a session runs.
    broker: Option<Arc<SeqPacket>>,
}

struct Gnss {
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Android's GNSS service wraps a successful but empty extension without
/// checking it (it crashed system_server in GnssPsdsAidl::setCallback), so
/// every missing extension, nullable or not, is an error.
fn unsupported<T>() -> binder::Result<T> {
    Err(Status::new_exception(
        ExceptionCode::UNSUPPORTED_OPERATION,
        None,
    ))
}

/// Nanoseconds since boot, which Android's locations carry.
fn boottime_ns() -> i64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime only writes the timespec it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) };
    now.tv_sec * 1_000_000_000 + now.tv_nsec
}

fn location_from(fix: &droidloom_location_protocol::Fix) -> GnssLocation {
    let mut flags = location::HAS_LAT_LONG | location::HAS_HORIZONTAL_ACCURACY;
    if fix.flags & HAS_ALTITUDE != 0 {
        flags |= location::HAS_ALTITUDE;
    }
    if fix.flags & HAS_SPEED != 0 {
        flags |= location::HAS_SPEED;
    }
    if fix.flags & HAS_BEARING != 0 {
        flags |= location::HAS_BEARING;
    }
    GnssLocation {
        gnssLocationFlags: flags,
        latitudeDegrees: fix.latitude,
        longitudeDegrees: fix.longitude,
        altitudeMeters: fix.altitude,
        speedMetersPerSec: f64::from(fix.speed),
        bearingDegrees: f64::from(fix.bearing),
        horizontalAccuracyMeters: f64::from(fix.accuracy),
        timestampMillis: fix.time_ms,
        elapsedRealtime: ElapsedRealtime {
            flags: elapsed::HAS_TIMESTAMP_NS,
            timestampNs: boottime_ns(),
            timeUncertaintyNs: 0.0,
        },
        ..Default::default()
    }
}

/// Reads the broker's records and reports them until the connection ends.
fn follow(state: Arc<Mutex<State>>, broker: Arc<SeqPacket>) {
    let mut bytes = [0_u8; RECORD_BYTES];
    loop {
        let record = match broker.receive_record_into(&mut bytes) {
            Ok(record) => Record::decode(record.bytes),
            Err(error) => {
                eprintln!("Droidloom GNSS: broker gone: {error}");
                break;
            }
        };
        let callback = lock(&state).callback.clone();
        match record {
            Ok(Record::Fix(fix)) => {
                if let Some(callback) = callback {
                    let _ = callback.gnssLocationCb(&location_from(&fix));
                }
            }
            Ok(Record::Denied) => eprintln!("Droidloom GNSS: the host refuses location"),
            Ok(_) | Err(_) => {
                eprintln!("Droidloom GNSS: unexpected record from the broker");
                break;
            }
        }
    }
    let mut state = lock(&state);
    if state
        .broker
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &broker))
    {
        state.broker = None;
    }
}

impl Gnss {
    /// Tells the broker to run at the current interval, connecting first.
    fn request(&self, state: &mut State) {
        let broker = match &state.broker {
            Some(broker) => broker.clone(),
            None => match SeqPacket::connect(BROKER) {
                Ok(socket) => {
                    let socket = Arc::new(socket);
                    let reader = socket.clone();
                    let shared = self.state.clone();
                    let _ = std::thread::Builder::new()
                        .name("gnss-broker".to_owned())
                        .spawn(move || follow(shared, reader));
                    state.broker = Some(socket.clone());
                    socket
                }
                Err(error) => {
                    eprintln!("Droidloom GNSS: no location broker: {error}");
                    return;
                }
            },
        };
        let _ = broker.send_record(&Record::Start(state.interval_ms).encode(), &[]);
    }

    fn report_status(state: &State, status: GnssStatusValue) {
        if let Some(callback) = &state.callback {
            let _ = callback.gnssStatusCb(status);
        }
    }
}

impl Interface for Gnss {
    fn dump(&self, writer: &mut dyn Write, _args: &[&CStr]) -> Result<(), binder::StatusCode> {
        let state = lock(&self.state);
        writeln!(
            writer,
            "active {} interval {} ms broker {}",
            state.active,
            state.interval_ms,
            state.broker.is_some()
        )
        .map_err(|_| binder::StatusCode::UNKNOWN_ERROR)
    }
}

#[allow(non_snake_case, reason = "AIDL method names")]
impl IGnss for Gnss {
    fn setCallback(&self, callback: &Strong<dyn IGnssCallback>) -> binder::Result<()> {
        callback.gnssSetCapabilitiesCb(callback::CAPABILITY_SCHEDULING)?;
        callback.gnssSetSystemInfoCb(&GnssSystemInfo {
            yearOfHw: 2026,
            name: "Droidloom host location".to_owned(),
        })?;
        let _ = callback.gnssSetSignalTypeCapabilitiesCb(&[] as &[GnssSignalType]);
        lock(&self.state).callback = Some(callback.clone());
        Ok(())
    }

    fn close(&self) -> binder::Result<()> {
        let mut state = lock(&self.state);
        state.active = false;
        state.callback = None;
        // Dropping the connection stops the host's session.
        state.broker = None;
        Ok(())
    }

    fn start(&self) -> binder::Result<()> {
        let mut state = lock(&self.state);
        state.active = true;
        self.request(&mut state);
        Self::report_status(&state, GnssStatusValue::SESSION_BEGIN);
        Ok(())
    }

    fn stop(&self) -> binder::Result<()> {
        let mut state = lock(&self.state);
        state.active = false;
        if let Some(broker) = &state.broker {
            let _ = broker.send_record(&Record::Stop.encode(), &[]);
        }
        Self::report_status(&state, GnssStatusValue::SESSION_END);
        Ok(())
    }

    fn setPositionMode(&self, options: &PositionModeOptions) -> binder::Result<()> {
        let mut state = lock(&self.state);
        state.interval_ms = u32::try_from(options.minIntervalMs)
            .unwrap_or(DEFAULT_INTERVAL_MS)
            .max(DEFAULT_INTERVAL_MS);
        if state.active {
            self.request(&mut state);
        }
        Ok(())
    }

    fn injectTime(&self, _time_ms: i64, _reference_ms: i64, _uncertainty_ms: i32) -> binder::Result<()> {
        Ok(())
    }

    fn injectLocation(&self, _location: &GnssLocation) -> binder::Result<()> {
        Ok(())
    }

    fn injectBestLocation(&self, _location: &GnssLocation) -> binder::Result<()> {
        Ok(())
    }

    fn deleteAidingData(&self, _flags: GnssAidingData) -> binder::Result<()> {
        Ok(())
    }

    fn startSvStatus(&self) -> binder::Result<()> {
        Ok(())
    }

    fn stopSvStatus(&self) -> binder::Result<()> {
        Ok(())
    }

    fn startNmea(&self) -> binder::Result<()> {
        Ok(())
    }

    fn stopNmea(&self) -> binder::Result<()> {
        Ok(())
    }

    fn getExtensionPsds(&self) -> binder::Result<Option<Strong<dyn IGnssPsds>>> {
        unsupported()
    }

    fn getExtensionGnssConfiguration(&self) -> binder::Result<Strong<dyn IGnssConfiguration>> {
        unsupported()
    }

    fn getExtensionGnssMeasurement(&self) -> binder::Result<Strong<dyn IGnssMeasurementInterface>> {
        unsupported()
    }

    fn getExtensionGnssPowerIndication(&self) -> binder::Result<Strong<dyn IGnssPowerIndication>> {
        unsupported()
    }

    fn getExtensionGnssBatching(&self) -> binder::Result<Option<Strong<dyn IGnssBatching>>> {
        unsupported()
    }

    fn getExtensionGnssGeofence(&self) -> binder::Result<Option<Strong<dyn IGnssGeofence>>> {
        unsupported()
    }

    fn getExtensionGnssNavigationMessage(
        &self,
    ) -> binder::Result<Option<Strong<dyn IGnssNavigationMessageInterface>>> {
        unsupported()
    }

    fn getExtensionAGnss(&self) -> binder::Result<Strong<dyn IAGnss>> {
        unsupported()
    }

    fn getExtensionAGnssRil(&self) -> binder::Result<Strong<dyn IAGnssRil>> {
        unsupported()
    }

    fn getExtensionGnssDebug(&self) -> binder::Result<Strong<dyn IGnssDebug>> {
        unsupported()
    }

    fn getExtensionGnssVisibilityControl(
        &self,
    ) -> binder::Result<Strong<dyn IGnssVisibilityControl>> {
        unsupported()
    }

    fn getExtensionGnssAntennaInfo(&self) -> binder::Result<Strong<dyn IGnssAntennaInfo>> {
        unsupported()
    }

    fn getExtensionMeasurementCorrections(
        &self,
    ) -> binder::Result<Option<Strong<dyn IMeasurementCorrectionsInterface>>> {
        unsupported()
    }

    fn getExtensionGnssAssistanceInterface(
        &self,
    ) -> binder::Result<Strong<dyn IGnssAssistanceInterface>> {
        unsupported()
    }
}

fn main() {
    ProcessState::set_thread_pool_max_thread_count(2);
    ProcessState::start_thread_pool();
    let gnss = Gnss {
        state: Arc::new(Mutex::new(State {
            callback: None,
            interval_ms: DEFAULT_INTERVAL_MS,
            active: false,
            broker: None,
        })),
    };
    let service = BnGnss::new_binder(gnss, BinderFeatures::default());
    if let Err(error) = binder::add_service(SERVICE_NAME, service.as_binder()) {
        eprintln!("Droidloom GNSS fatal: register {SERVICE_NAME}: {error:?}");
        std::process::exit(1);
    }
    ProcessState::join_thread_pool();
}
