//! Android NFC AIDL V2 service for Droidloom: the host's NFC controller.
//!
//! Android's NFC stack speaks NCI itself; this service carries its packets
//! to and from `/dev/nfc-nci`, a host controller that hands out raw NCI one
//! packet per read or write (the supervisor creates the node in the cell
//! when the host has one). Opening the node powers the controller and makes
//! it ready for NCI; closing it powers it off. The host's secure element is
//! not offered: everything routes to the host, for reading tags and card
//! emulation by apps.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use android_hardware_nfc::aidl::android::hardware::nfc::{
    INfc::{BnNfc, INfc},
    INfcClientCallback::INfcClientCallback,
    NfcCloseType::NfcCloseType,
    NfcConfig::NfcConfig,
    NfcEvent::NfcEvent,
    NfcStatus::NfcStatus,
    PresenceCheckAlgorithm::PresenceCheckAlgorithm,
    ProtocolDiscoveryConfig::ProtocolDiscoveryConfig,
};
use binder::{BinderFeatures, ExceptionCode, Interface, ProcessState, Status, Strong};

const SERVICE_NAME: &str = "android.hardware.nfc.INfc/default";
const DEVICE: &str = "/dev/nfc-nci";
/// An NCI header and the largest payload.
const MAX_PACKET: usize = 3 + 255;
/// How often the reader looks whether it should stop.
const POLL_MS: i32 = 200;

struct Link {
    device: Arc<File>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[derive(Default)]
struct State {
    callback: Option<Strong<dyn INfcClientCallback>>,
    link: Option<Link>,
    verbose: bool,
}

#[derive(Default)]
struct Nfc {
    state: Arc<Mutex<State>>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn failed(message: &str) -> Status {
    Status::new_service_specific_error_str(NfcStatus::FAILED.0, Some(message))
}

/// Waits up to `POLL_MS` for the device to have a packet.
fn readable(device: &File) -> bool {
    let mut poll = libc::pollfd {
        fd: device.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd, owned by this frame for the whole call.
    let ready = unsafe { libc::poll(&mut poll, 1, POLL_MS) };
    ready > 0 && poll.revents & libc::POLLIN != 0
}

/// Hands every packet from the controller to Android until stopped.
fn follow(device: Arc<File>, stop: Arc<AtomicBool>, state: Arc<Mutex<State>>) {
    let mut packet = [0_u8; MAX_PACKET];
    while !stop.load(Ordering::SeqCst) {
        if !readable(&device) {
            continue;
        }
        let length = match (&*device).read(&mut packet) {
            Ok(length) if length > 0 => length,
            Ok(_) => continue,
            Err(error) => {
                eprintln!("Droidloom NFC: read: {error}");
                if let Some(callback) = lock(&state).callback.clone() {
                    let _ = callback.sendEvent(NfcEvent::ERROR, NfcStatus::ERR_TRANSPORT);
                }
                break;
            }
        };
        let callback = lock(&state).callback.clone();
        if let Some(callback) = callback {
            let _ = callback.sendData(&packet[..length]);
        }
    }
}

/// Splits what Android writes into NCI packets, which the device takes one
/// per write.
fn packets(data: &[u8]) -> Option<Vec<&[u8]>> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let length = 3 + usize::from(*rest.get(2)?);
        if rest.len() < length {
            return None;
        }
        let (packet, after) = rest.split_at(length);
        out.push(packet);
        rest = after;
    }
    Some(out)
}

impl Nfc {
    fn connect(&self, state: &mut State) -> binder::Result<()> {
        if state.link.is_some() {
            return Ok(());
        }
        let device = OpenOptions::new()
            .read(true)
            .write(true)
            .open(DEVICE)
            .map_err(|error| {
                eprintln!("Droidloom NFC: open {DEVICE}: {error}");
                failed("no NFC controller")
            })?;
        let device = Arc::new(device);
        let stop = Arc::new(AtomicBool::new(false));
        let reader = {
            let (device, stop, shared) = (device.clone(), stop.clone(), self.state.clone());
            std::thread::Builder::new()
                .name("nfc-reader".to_owned())
                .spawn(move || follow(device, stop, shared))
                .map_err(|_| failed("no reader thread"))?
        };
        state.link = Some(Link {
            device,
            stop,
            reader: Some(reader),
        });
        Ok(())
    }

    /// Reports `event` from a thread of its own, after the call that caused
    /// it has returned.
    fn report(&self, event: NfcEvent, status: NfcStatus) {
        let state = self.state.clone();
        std::thread::spawn(move || {
            let callback = lock(&state).callback.clone();
            if let Some(callback) = callback {
                let _ = callback.sendEvent(event, status);
            }
        });
    }
}

impl Interface for Nfc {
    fn dump(&self, writer: &mut dyn Write, _args: &[&CStr]) -> Result<(), binder::StatusCode> {
        let state = lock(&self.state);
        writeln!(writer, "open {} verbose {}", state.link.is_some(), state.verbose)
            .map_err(|_| binder::StatusCode::UNKNOWN_ERROR)
    }
}

#[allow(non_snake_case, reason = "AIDL method names")]
impl INfc for Nfc {
    fn open(&self, callback: &Strong<dyn INfcClientCallback>) -> binder::Result<()> {
        let mut state = lock(&self.state);
        state.callback = Some(callback.clone());
        match self.connect(&mut state) {
            Ok(()) => self.report(NfcEvent::OPEN_CPLT, NfcStatus::OK),
            Err(_) => self.report(NfcEvent::OPEN_CPLT, NfcStatus::FAILED),
        }
        Ok(())
    }

    fn close(&self, _type: NfcCloseType) -> binder::Result<()> {
        let link = lock(&self.state).link.take();
        drop(link);
        self.report(NfcEvent::CLOSE_CPLT, NfcStatus::OK);
        Ok(())
    }

    fn coreInitialized(&self) -> binder::Result<()> {
        self.report(NfcEvent::POST_INIT_CPLT, NfcStatus::OK);
        Ok(())
    }

    fn factoryReset(&self) -> binder::Result<()> {
        Ok(())
    }

    #[allow(clippy::cast_possible_wrap, reason = "AIDL bytes are signed")]
    fn getConfig(&self) -> binder::Result<NfcConfig> {
        // From the controller's Android configuration (libnfc-hal-st.conf),
        // with every route on the host: the secure element is not offered.
        Ok(NfcConfig {
            nfaPollBailOutMode: true,
            presenceCheckAlgorithm: PresenceCheckAlgorithm::I_BLOCK,
            nfaProprietaryCfg: ProtocolDiscoveryConfig {
                protocol18092Active: 0x05,
                protocolBPrime: 0xff_u8 as i8,
                protocolDual: 0xff_u8 as i8,
                protocol15693: 0x06,
                protocolKovio: 0x8a_u8 as i8,
                protocolMifare: 0x90_u8 as i8,
                discoveryPollKovio: 0x77,
                discoveryPollBPrime: 0xff_u8 as i8,
                discoveryListenBPrime: 0xff_u8 as i8,
                protocolChineseId: 0xff_u8 as i8,
            },
            maxIsoDepTransceiveLength: 0xfeff,
            ..Default::default()
        })
    }

    fn powerCycle(&self) -> binder::Result<()> {
        let mut state = lock(&self.state);
        drop(state.link.take());
        match self.connect(&mut state) {
            Ok(()) => self.report(NfcEvent::OPEN_CPLT, NfcStatus::OK),
            Err(_) => self.report(NfcEvent::ERROR, NfcStatus::ERR_TRANSPORT),
        }
        Ok(())
    }

    fn preDiscover(&self) -> binder::Result<()> {
        self.report(NfcEvent::PRE_DISCOVER_CPLT, NfcStatus::OK);
        Ok(())
    }

    fn write(&self, data: &[u8]) -> binder::Result<i32> {
        let device = lock(&self.state)
            .link
            .as_ref()
            .map(|link| link.device.clone())
            .ok_or_else(|| Status::new_exception(ExceptionCode::ILLEGAL_STATE, None))?;
        let packets = packets(data)
            .ok_or_else(|| Status::new_exception(ExceptionCode::ILLEGAL_ARGUMENT, None))?;
        for packet in packets {
            if let Err(error) = (&*device).write_all(packet) {
                eprintln!("Droidloom NFC: write: {error}");
                return Err(failed("write failed"));
            }
        }
        Ok(i32::try_from(data.len()).unwrap_or(i32::MAX))
    }

    fn setEnableVerboseLogging(&self, enable: bool) -> binder::Result<()> {
        lock(&self.state).verbose = enable;
        Ok(())
    }

    fn isVerboseLoggingEnabled(&self) -> binder::Result<bool> {
        Ok(lock(&self.state).verbose)
    }

    fn controlGranted(&self) -> binder::Result<NfcStatus> {
        Ok(NfcStatus::OK)
    }
}

fn main() {
    ProcessState::set_thread_pool_max_thread_count(2);
    ProcessState::start_thread_pool();
    let service = BnNfc::new_binder(Nfc::default(), BinderFeatures::default());
    if let Err(error) = binder::add_service(SERVICE_NAME, service.as_binder()) {
        eprintln!("Droidloom NFC fatal: register {SERVICE_NAME}: {error:?}");
        std::process::exit(1);
    }
    ProcessState::join_thread_pool();
}

#[cfg(test)]
mod tests {
    use super::packets;

    #[test]
    fn writes_split_into_packets() {
        let two = [0x20, 0x00, 0x01, 0x00, 0x20, 0x01, 0x02, 0x00, 0x00];
        assert_eq!(packets(&two).unwrap().len(), 2);
        assert!(packets(&[0x20, 0x00, 0x05, 0x00]).is_none());
        assert!(packets(&[]).unwrap().is_empty());
    }
}
