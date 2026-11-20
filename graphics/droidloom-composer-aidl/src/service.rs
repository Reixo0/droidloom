//! Binder-facing Composer V5 service and client.
//!
//! This module provides the embeddable Binder object and serialized task-display
//! lifecycle. The Soong-only Droidloom Composer executable instantiates it with
//! the concrete minigbm/Denial adapter; product installation remains gated on
//! the Android task bridge, init service, and VINTF declaration.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::marker::PhantomData;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex, MutexGuard,
};
use std::time::Duration;

use android_hardware_common::aidl::android::hardware::common::NativeHandle::NativeHandle;
use android_hardware_drm_common::aidl::android::hardware::drm::HdcpLevels::HdcpLevels;
use android_hardware_graphics_common::aidl::android::hardware::graphics::common::{
    Dataspace::Dataspace, DisplayDecorationSupport::DisplayDecorationSupport,
    DisplayHotplugEvent::DisplayHotplugEvent, Hdr::Hdr,
    HdrConversionCapability::HdrConversionCapability, HdrConversionStrategy::HdrConversionStrategy,
    PixelFormat::PixelFormat, Transform::Transform,
};
use android_hardware_graphics_composer3::aidl::android::hardware::graphics::composer3::{
    Buffer::Buffer,
    Capability::Capability,
    ClockMonotonicTimestamp::ClockMonotonicTimestamp,
    ColorMode::ColorMode,
    CommandResultPayload::CommandResultPayload,
    ContentType::ContentType,
    DisplayAttribute::DisplayAttribute,
    DisplayCapability::DisplayCapability,
    DisplayCommand::DisplayCommand,
    DisplayConfiguration::DisplayConfiguration,
    DisplayConnectionType::DisplayConnectionType,
    DisplayContentSample::DisplayContentSample,
    DisplayContentSamplingAttributes::DisplayContentSamplingAttributes,
    DisplayIdentification::DisplayIdentification,
    FormatColorComponent::FormatColorComponent,
    HdrCapabilities::HdrCapabilities,
    IComposer::{BnComposer, IComposer},
    IComposerCallback::IComposerCallback,
    IComposerClient::{BnComposerClient, IComposerClient},
    Luts::Luts,
    OutputType::OutputType,
    OverlayProperties::OverlayProperties,
    PerFrameMetadataKey::PerFrameMetadataKey,
    PowerMode::PowerMode as AidlPowerMode,
    ReadbackBufferAttributes::ReadbackBufferAttributes,
    RenderIntent::RenderIntent,
    VirtualDisplay::VirtualDisplay,
    VsyncPeriodChangeConstraints::VsyncPeriodChangeConstraints,
    VsyncPeriodChangeTimeline::VsyncPeriodChangeTimeline,
    VsyncSample::VsyncSample,
};
use binder::{BinderFeatures, Interface, Strong};
use droidloom_composer::{
    hwc3::{ErrorCode, Session, MAX_BUFFER_SLOTS},
    DisplayId, LayerId, PowerMode, ReservedTaskWindowSpec, TaskId, TaskWindowSpec,
};
use droidloom_syncobj::frame_trace;
use droidloom_syncobj::monotonic_deadline_after;
use droidloom_transport::Configure;

use crate::{execute_commands, NativeBufferAdapter};

const CONFIG_ID: i32 = 0;
/// The low mode offered next to the host's rate. Denial's automatic refresh
/// rate drops panels that switch rates live to 60 Hz; with both modes,
/// SurfaceFlinger chooses per content (a 60 fps video, touch, animations)
/// as on a phone, and the choice reaches the host as the task's frame rate.
const LOW_CONFIG_ID: i32 = 1;
const LOW_REFRESH_MILLIHZ: u32 = 60_000;
const CONFIG_GROUP: i32 = 0;
const UNKNOWN_DPI_MILLI: i32 = -1;

/// Binder-visible state shared with the future Android-task lifecycle bridge.
pub type SharedSession = Arc<Mutex<Session>>;

type SharedCallback = Arc<Mutex<Option<Strong<dyn IComposerCallback>>>>;
/// Told each task display's chosen refresh rate in millihertz.
pub type FrameRateListener = Box<dyn Fn(DisplayId, u32) + Send + Sync>;

/// Which displays SurfaceFlinger has switched to the low mode, and who
/// hears about it.
#[derive(Default)]
struct DisplayModes {
    low: Mutex<BTreeSet<DisplayId>>,
    listener: Mutex<Option<FrameRateListener>>,
}
type SharedModes = Arc<DisplayModes>;

impl DisplayModes {
    fn is_low(&self, display: DisplayId) -> bool {
        self.low.lock().is_ok_and(|low| low.contains(&display))
    }

    /// The configs one display offers: the host's rate, and 60 Hz below it.
    fn configs(configure: Configure) -> binder::Result<Vec<(i32, u64)>> {
        let mut configs = vec![(CONFIG_ID, configure_refresh_period(configure)?)];
        if u64::from(configure.refresh_millihz) * 100 > u64::from(LOW_REFRESH_MILLIHZ) * 101 {
            configs.push((
                LOW_CONFIG_ID,
                1_000_000_000_000_u64 / u64::from(LOW_REFRESH_MILLIHZ),
            ));
        }
        Ok(configs)
    }

    fn config_period(configure: Configure, config: i32) -> binder::Result<u64> {
        Self::configs(configure)?
            .into_iter()
            .find(|(id, _)| *id == config)
            .map(|(_, period)| period)
            .ok_or_else(|| service_error(ErrorCode::BadConfig))
    }

    fn active_config(&self, display: DisplayId, configure: Configure) -> binder::Result<i32> {
        let low = self.is_low(display)
            && Self::configs(configure)?
                .iter()
                .any(|(id, _)| *id == LOW_CONFIG_ID);
        Ok(if low { LOW_CONFIG_ID } else { CONFIG_ID })
    }

    fn active_period(&self, display: DisplayId, configure: Configure) -> binder::Result<u64> {
        Self::config_period(configure, self.active_config(display, configure)?)
    }

    /// Switch a display's mode; returns whether it changed.
    fn set_active(&self, display: DisplayId, config: i32) -> binder::Result<bool> {
        let mut low = lock(&self.low)?;
        Ok(if config == LOW_CONFIG_ID {
            low.insert(display)
        } else {
            low.remove(&display)
        })
    }

    fn forget(&self, display: DisplayId) {
        if let Ok(mut low) = self.low.lock() {
            low.remove(&display);
        }
    }

    fn announce(&self, display: DisplayId, millihz: u32) {
        if let Ok(listener) = self.listener.lock() {
            if let Some(listener) = listener.as_ref() {
                listener(display, millihz);
            }
        }
    }
}
type ConnectedDisplays = Arc<Mutex<BTreeSet<DisplayId>>>;

/// SurfaceFlinger wakes one period minus its default 1 ms phase offset
/// before the vsync it composes for.
const SF_PHASE_OFFSET_NANOS: u64 = 1_000_000;
/// Time SurfaceFlinger gets to compose and hand a frame to the presenter
/// before the host's latch.
const SF_LATCH_BUDGET_NANOS: u64 = 2_000_000;

// With a host clock, vsync follows the host's frame grants and there is no
// timer thread. Without one, a free-running timer seeded from presentation
// feedback stands in.
#[derive(Clone)]
struct VsyncScheduler {
    shared: Arc<VsyncSchedulerShared>,
    host_clock: bool,
}

struct VsyncSchedulerShared {
    callback: SharedCallback,
    schedule: Mutex<VsyncSchedule>,
    wake: Condvar,
    /// The `audit` diagnostic, read once at startup.
    audit: bool,
}

#[derive(Default)]
struct VsyncSchedule {
    displays: BTreeMap<DisplayId, ScheduledVsync>,
    presentation_phase: Option<(u64, u64)>,
    /// Only with the `audit` diagnostic.
    audit: Option<VsyncAudit>,
}

#[derive(Clone, Copy)]
struct ScheduledVsync {
    period_nanos: u64,
    next_nanos: u64,
    // False until a host sample at this period has set the phase.
    phased: bool,
}

impl ScheduledVsync {
    // Start on the host's phase when a sample at this period exists; otherwise
    // run one period out and take the phase from the first sample that does.
    fn start(now: u64, period_nanos: u64, phase: Option<(u64, u64)>) -> Self {
        match phase.filter(|(_, phase_period)| periods_compatible(period_nanos, *phase_period)) {
            Some((phase, _)) => Self {
                period_nanos,
                next_nanos: next_vsync_after(now, phase, period_nanos),
                phased: true,
            },
            None => Self {
                period_nanos,
                next_nanos: now.saturating_add(period_nanos),
                phased: false,
            },
        }
    }

    fn observe(&mut self, now: u64, timestamp_nanos: u64, period_nanos: u64) {
        if !self.phased && periods_compatible(self.period_nanos, period_nanos) {
            self.next_nanos = next_vsync_after(now, timestamp_nanos, self.period_nanos);
            self.phased = true;
        }
    }
}

#[derive(Default)]
struct VsyncAudit {
    window_started_nanos: u64,
    scheduler_ticks: u64,
    skipped_periods: u64,
    wake_late_total_nanos: u128,
    wake_late_max_nanos: u64,
    callback_total_nanos: u128,
    callback_max_nanos: u64,
    presentations: u64,
    duplicate_presentations: u64,
    presentation_interval_count: u64,
    presentation_interval_total_nanos: u128,
    presentation_interval_max_nanos: u64,
    delivery_late_total_nanos: u128,
    delivery_late_max_nanos: u64,
    phase_observations: u64,
    phase_error_total_nanos: u128,
    phase_error_max_nanos: u64,
    last_presentation_nanos: Option<u64>,
}

impl VsyncScheduler {
    fn new(callback: SharedCallback, host_clock: bool) -> Self {
        let audit = frame_trace::traced(frame_trace::Diagnostic::Audit);
        let shared = Arc::new(VsyncSchedulerShared {
            callback,
            schedule: Mutex::new(VsyncSchedule {
                audit: audit.then(VsyncAudit::default),
                ..VsyncSchedule::default()
            }),
            wake: Condvar::new(),
            audit,
        });
        if !host_clock {
            let worker = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("droidloom-vsync".to_owned())
                .spawn(move || run_vsync_scheduler(&worker))
                .expect("create Droidloom vsync scheduler");
        }
        Self { shared, host_clock }
    }

    /// Deliver one host frame grant as a vsync to every enabled display at
    /// its period. The sample is the vblank before the grant's latch, moved
    /// early by SurfaceFlinger's phase offset and latch budget, so SF wakes
    /// that budget before the host latches. The host sends no grants while
    /// Android is idle or the display is off; SF keeps its model meanwhile.
    fn frame_grant(&self, latch_nanos: u64, period_nanos: u64) {
        if !self.host_clock {
            return;
        }
        let Some(timestamp_nanos) =
            latch_nanos.checked_sub(period_nanos + SF_PHASE_OFFSET_NANOS + SF_LATCH_BUDGET_NANOS)
        else {
            return;
        };
        let Ok(mut schedule) = self.shared.schedule.lock() else {
            return;
        };
        let due = schedule
            .displays
            .iter()
            .filter(|(_, scheduled)| periods_compatible(scheduled.period_nanos, period_nanos))
            .map(|(display, scheduled)| (*display, timestamp_nanos, scheduled.period_nanos))
            .collect::<Vec<_>>();
        if due.is_empty() {
            return;
        }
        // In host-clock mode the wake columns time grant delivery from the
        // host tick that issued it.
        if let (Some(audit), Ok(now)) = (schedule.audit.as_mut(), monotonic_timestamp_nanos()) {
            if audit.window_started_nanos == 0 {
                audit.window_started_nanos = now;
            }
            let delivery = now.saturating_sub(latch_nanos.saturating_sub(period_nanos));
            audit.scheduler_ticks = audit.scheduler_ticks.saturating_add(1);
            audit.wake_late_total_nanos = audit
                .wake_late_total_nanos
                .saturating_add(u128::from(delivery));
            audit.wake_late_max_nanos = audit.wake_late_max_nanos.max(delivery);
        }
        drop(schedule);
        deliver_vsyncs(&self.shared, due);
    }

    fn set_enabled(
        &self,
        display: DisplayId,
        enabled: bool,
        period_nanos: u64,
    ) -> binder::Result<()> {
        if period_nanos == 0 {
            return Err(service_error(ErrorCode::BadConfig));
        }
        let now = monotonic_timestamp_nanos()?;
        let mut schedule = lock(&self.shared.schedule)?;
        if enabled {
            let scheduled = ScheduledVsync::start(now, period_nanos, schedule.presentation_phase);
            schedule.displays.insert(display, scheduled);
        } else {
            schedule.displays.remove(&display);
        }
        drop(schedule);
        self.shared.wake.notify_one();
        Ok(())
    }

    fn update_period(&self, display: DisplayId, period_nanos: u64) -> binder::Result<()> {
        if period_nanos == 0 {
            return Err(service_error(ErrorCode::BadConfig));
        }
        let now = monotonic_timestamp_nanos()?;
        let mut schedule = lock(&self.shared.schedule)?;
        let phase = schedule.presentation_phase;
        if let Some(active) = schedule.displays.get_mut(&display) {
            if active.period_nanos == period_nanos {
                return Ok(());
            }
            *active = ScheduledVsync::start(now, period_nanos, phase);
        }
        // A configure must not enable a clock disabled by SurfaceFlinger.
        drop(schedule);
        self.shared.wake.notify_one();
        Ok(())
    }

    fn disable(&self, display: DisplayId) {
        if let Ok(mut schedule) = self.shared.schedule.lock() {
            schedule.displays.remove(&display);
        }
        self.shared.wake.notify_one();
    }

    fn clear(&self) {
        if let Ok(mut schedule) = self.shared.schedule.lock() {
            schedule.displays.clear();
        }
        self.shared.wake.notify_one();
    }

    fn observe_presentation(&self, timestamp_nanos: u64, period_nanos: u64) {
        if period_nanos == 0 || self.host_clock {
            return;
        }
        let Ok(now) = monotonic_timestamp_nanos() else {
            return;
        };
        let Ok(mut schedule) = self.shared.schedule.lock() else {
            return;
        };
        let state = &mut *schedule;
        if let Some(audit) = state.audit.as_mut() {
            audit_presentation(audit, &state.displays, now, timestamp_nanos, period_nanos);
        }
        schedule.presentation_phase = Some((timestamp_nanos, period_nanos));

        // Presentation feedback is delivered well after the physical vblank.
        // Moving a phased clock here could delete its imminent tick, so only
        // a clock still waiting for its first sample at this period moves.
        for scheduled in schedule.displays.values_mut() {
            scheduled.observe(now, timestamp_nanos, period_nanos);
        }
        drop(schedule);
        self.shared.wake.notify_one();
    }
}

fn audit_presentation(
    audit: &mut VsyncAudit,
    displays: &BTreeMap<DisplayId, ScheduledVsync>,
    now: u64,
    timestamp_nanos: u64,
    period_nanos: u64,
) {
    if audit.window_started_nanos == 0 {
        audit.window_started_nanos = now;
    }
    audit.presentations = audit.presentations.saturating_add(1);
    let delivery_late = now.saturating_sub(timestamp_nanos);
    audit.delivery_late_total_nanos = audit
        .delivery_late_total_nanos
        .saturating_add(u128::from(delivery_late));
    audit.delivery_late_max_nanos = audit.delivery_late_max_nanos.max(delivery_late);
    if let Some(previous) = audit.last_presentation_nanos {
        if timestamp_nanos == previous {
            audit.duplicate_presentations = audit.duplicate_presentations.saturating_add(1);
        } else if timestamp_nanos > previous {
            let interval = timestamp_nanos - previous;
            audit.presentation_interval_count = audit.presentation_interval_count.saturating_add(1);
            audit.presentation_interval_total_nanos = audit
                .presentation_interval_total_nanos
                .saturating_add(u128::from(interval));
            audit.presentation_interval_max_nanos =
                audit.presentation_interval_max_nanos.max(interval);
        }
    }
    audit.last_presentation_nanos = Some(timestamp_nanos);
    let mut phase_observations = 0_u64;
    let mut phase_error_total_nanos = 0_u128;
    let mut phase_error_max_nanos = 0_u64;
    for scheduled in displays.values() {
        if periods_compatible(scheduled.period_nanos, period_nanos) {
            let host_next = next_vsync_after(now, timestamp_nanos, scheduled.period_nanos);
            let phase_error =
                phase_distance(host_next, scheduled.next_nanos, scheduled.period_nanos);
            phase_observations = phase_observations.saturating_add(1);
            phase_error_total_nanos =
                phase_error_total_nanos.saturating_add(u128::from(phase_error));
            phase_error_max_nanos = phase_error_max_nanos.max(phase_error);
        }
    }
    audit.phase_observations = audit.phase_observations.saturating_add(phase_observations);
    audit.phase_error_total_nanos = audit
        .phase_error_total_nanos
        .saturating_add(phase_error_total_nanos);
    audit.phase_error_max_nanos = audit.phase_error_max_nanos.max(phase_error_max_nanos);
}

fn run_vsync_scheduler(shared: &VsyncSchedulerShared) {
    loop {
        let Ok(mut schedule) = shared.schedule.lock() else {
            eprintln!("Droidloom vsync scheduler state is poisoned");
            return;
        };
        while schedule.displays.is_empty() {
            let Ok(guard) = shared.wake.wait(schedule) else {
                eprintln!("Droidloom vsync scheduler wait failed");
                return;
            };
            schedule = guard;
        }

        let Ok(now) = monotonic_timestamp_nanos() else {
            drop(schedule);
            std::thread::sleep(Duration::from_millis(1));
            continue;
        };
        let Some(next) = schedule
            .displays
            .values()
            .map(|display| display.next_nanos)
            .min()
        else {
            continue;
        };
        if next > now {
            let timeout = Duration::from_nanos(next - now);
            let Ok((guard, _)) = shared.wake.wait_timeout(schedule, timeout) else {
                eprintln!("Droidloom vsync scheduler timed wait failed");
                return;
            };
            drop(guard);
            continue;
        }

        let mut due = Vec::new();
        let mut scheduler_ticks = 0_u64;
        let mut skipped_periods = 0_u64;
        let mut wake_late_total_nanos = 0_u128;
        let mut wake_late_max_nanos = 0_u64;
        for (display, scheduled) in &mut schedule.displays {
            if scheduled.next_nanos <= now {
                let timestamp_nanos = scheduled.next_nanos;
                let wake_late = now - timestamp_nanos;
                let skipped = wake_late / scheduled.period_nanos;
                scheduled.next_nanos =
                    next_vsync_after(now, scheduled.next_nanos, scheduled.period_nanos);
                due.push((*display, timestamp_nanos, scheduled.period_nanos));
                scheduler_ticks = scheduler_ticks.saturating_add(1);
                skipped_periods = skipped_periods.saturating_add(skipped);
                wake_late_total_nanos = wake_late_total_nanos.saturating_add(u128::from(wake_late));
                wake_late_max_nanos = wake_late_max_nanos.max(wake_late);
            }
        }
        if let Some(audit) = schedule.audit.as_mut() {
            if audit.window_started_nanos == 0 {
                audit.window_started_nanos = now;
            }
            audit.scheduler_ticks = audit.scheduler_ticks.saturating_add(scheduler_ticks);
            audit.skipped_periods = audit.skipped_periods.saturating_add(skipped_periods);
            audit.wake_late_total_nanos = audit
                .wake_late_total_nanos
                .saturating_add(wake_late_total_nanos);
            audit.wake_late_max_nanos = audit.wake_late_max_nanos.max(wake_late_max_nanos);
        }
        drop(schedule);
        deliver_vsyncs(shared, due);
    }
}

fn deliver_vsyncs(shared: &VsyncSchedulerShared, due: Vec<(DisplayId, u64, u64)>) {
    let callback = shared
        .callback
        .lock()
        .ok()
        .and_then(|callback| callback.clone());
    let Some(callback) = callback else {
        return;
    };
    let mut failed = Vec::new();
    let mut callback_total_nanos = 0_u128;
    let mut callback_max_nanos = 0_u64;
    for (display, timestamp_nanos, period_nanos) in due {
        let Ok(display_value) = i64::try_from(display.0) else {
            failed.push(display);
            continue;
        };
        let Ok(timestamp_value) = i64::try_from(timestamp_nanos) else {
            failed.push(display);
            continue;
        };
        let Ok(period_value) = i32::try_from(period_nanos) else {
            failed.push(display);
            continue;
        };
        let callback_started = shared
            .audit
            .then(monotonic_timestamp_nanos)
            .and_then(Result::ok);
        let result = callback.onVsync(display_value, timestamp_value, period_value);
        if let (Some(started), Ok(finished)) = (
            callback_started,
            callback_started.map_or(Ok(0), |_| monotonic_timestamp_nanos()),
        ) {
            let elapsed = finished.saturating_sub(started);
            callback_total_nanos = callback_total_nanos.saturating_add(u128::from(elapsed));
            callback_max_nanos = callback_max_nanos.max(elapsed);
        }
        if let Err(error) = result {
            eprintln!("Droidloom vsync callback failed for {display:?}: {error:?}");
            failed.push(display);
        }
    }
    if !shared.audit && failed.is_empty() {
        return;
    }
    if let Ok(mut schedule) = shared.schedule.lock() {
        for display in failed {
            schedule.displays.remove(&display);
        }
        if let Some(audit) = schedule.audit.as_mut() {
            audit.callback_total_nanos = audit
                .callback_total_nanos
                .saturating_add(callback_total_nanos);
            audit.callback_max_nanos = audit.callback_max_nanos.max(callback_max_nanos);
            if let Ok(now) = monotonic_timestamp_nanos() {
                maybe_log_vsync_audit(audit, now);
            }
        }
    }
}

fn maybe_log_vsync_audit(audit: &mut VsyncAudit, now: u64) {
    const WINDOW_NANOS: u64 = 1_000_000_000;
    if audit.window_started_nanos == 0 {
        audit.window_started_nanos = now;
        return;
    }
    let elapsed = now.saturating_sub(audit.window_started_nanos);
    if elapsed < WINDOW_NANOS {
        return;
    }
    let mut line = format!(
        "Droidloom vsync audit: interval_ms={} ticks={} skipped={} wake_late_avg_us={} wake_late_max_us={} callback_avg_us={} callback_max_us={} presentations={} duplicate_presentations={} present_interval_avg_us={} present_interval_max_us={} delivery_late_avg_us={} delivery_late_max_us={} phase_observations={} phase_error_avg_us={} phase_error_max_us={}",
        elapsed / 1_000_000,
        audit.scheduler_ticks,
        audit.skipped_periods,
        average_micros(audit.wake_late_total_nanos, audit.scheduler_ticks),
        audit.wake_late_max_nanos / 1_000,
        average_micros(audit.callback_total_nanos, audit.scheduler_ticks),
        audit.callback_max_nanos / 1_000,
        audit.presentations,
        audit.duplicate_presentations,
        average_micros(
            audit.presentation_interval_total_nanos,
            audit.presentation_interval_count,
        ),
        audit.presentation_interval_max_nanos / 1_000,
        average_micros(audit.delivery_late_total_nanos, audit.presentations),
        audit.delivery_late_max_nanos / 1_000,
        audit.phase_observations,
        average_micros(
            audit.phase_error_total_nanos,
            audit.phase_observations,
        ),
        audit.phase_error_max_nanos / 1_000,
    );
    line.push('\n');
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
    let last_presentation_nanos = audit.last_presentation_nanos;
    *audit = VsyncAudit {
        window_started_nanos: now,
        last_presentation_nanos,
        ..VsyncAudit::default()
    };
}

fn average_micros(total_nanos: u128, count: u64) -> u128 {
    if count == 0 {
        return 0;
    }
    total_nanos / u128::from(count) / 1_000
}

fn monotonic_timestamp_nanos() -> binder::Result<u64> {
    monotonic_deadline_after(Duration::ZERO)
        .map_err(|_| service_error(ErrorCode::NoResources))
        .and_then(|timestamp| {
            u64::try_from(timestamp).map_err(|_| service_error(ErrorCode::NoResources))
        })
}

fn next_vsync_after(now: u64, phase: u64, period: u64) -> u64 {
    if phase > now {
        return phase;
    }
    let periods = now.saturating_sub(phase) / period;
    phase.saturating_add(periods.saturating_add(1).saturating_mul(period))
}

fn phase_distance(first: u64, second: u64, period: u64) -> u64 {
    let remainder = first.abs_diff(second) % period;
    remainder.min(period - remainder)
}

fn periods_compatible(first: u64, second: u64) -> bool {
    first.abs_diff(second) <= (first / 100).max(1)
}

/// Lifecycle handle used by the Android task bridge to stage and hotplug one
/// independent Composer display per top-level Android task.
#[derive(Clone)]
pub struct ComposerLifecycle {
    session: SharedSession,
    callback: SharedCallback,
    connected: ConnectedDisplays,
    gate: Arc<Mutex<()>>,
    vsync: VsyncScheduler,
    modes: SharedModes,
}

impl ComposerLifecycle {
    /// Stage one task/display mapping before Denial supplies format feedback
    /// and an initial configure. It is not visible to `SurfaceFlinger` yet.
    pub fn stage_task(&self, reservation: ReservedTaskWindowSpec) -> binder::Result<()> {
        let _gate = lock(&self.gate)?;
        lock(&self.session)?
            .windows_mut()
            .reserve(reservation)
            .map_err(|_| service_error(ErrorCode::BadParameter))
    }

    /// Bind the real ActivityTaskManager task ID after launch on a connected
    /// staged display.
    pub fn bind_task(&self, display: DisplayId, task: TaskId) -> binder::Result<TaskWindowSpec> {
        let _gate = lock(&self.gate)?;
        if !lock(&self.connected)?.contains(&display) {
            return Err(service_error(ErrorCode::BadDisplay));
        }
        lock(&self.session)?
            .windows_mut()
            .bind_task(display, task)
            .map_err(|_| service_error(ErrorCode::BadParameter))
    }

    /// Bind a task to an internal direct-presentation channel without
    /// announcing that channel as an Android display to SurfaceFlinger.
    pub fn bind_direct_task(
        &self,
        display: DisplayId,
        task: TaskId,
    ) -> binder::Result<TaskWindowSpec> {
        let _gate = lock(&self.gate)?;
        if lock(&self.connected)?.contains(&display) {
            return Err(service_error(ErrorCode::BadDisplay));
        }
        let mut session = lock(&self.session)?;
        let windows = session.windows_mut();
        let spec = windows
            .bind_task(display, task)
            .map_err(|_| service_error(ErrorCode::BadParameter))?;
        windows
            .composer_for_display_mut(display)
            .map_err(|_| service_error(ErrorCode::BadDisplay))?
            .set_power_mode(display, PowerMode::On)
            .map_err(|_| service_error(ErrorCode::NoResources))?;
        Ok(spec)
    }

    /// Publish a staged and configured task display as connected.
    pub fn connect_display(&self, display: DisplayId) -> binder::Result<()> {
        let _gate = lock(&self.gate)?;
        let aidl_display =
            i64::try_from(display.0).map_err(|_| service_error(ErrorCode::BadDisplay))?;
        if lock(&self.session)?
            .windows()
            .composer_for_display(display)
            .map_err(|_| service_error(ErrorCode::BadDisplay))?
            .latest_configure()
            .is_none()
        {
            return Err(service_error(ErrorCode::BadConfig));
        }
        if !lock(&self.connected)?.insert(display) {
            return Err(service_error(ErrorCode::BadDisplay));
        }
        if let Some(callback) = lock(&self.callback)?.as_ref() {
            callback.onHotplugEvent(aidl_display, DisplayHotplugEvent::CONNECTED)?;
        }
        Ok(())
    }

    /// Disconnect and remove one task display after every Denial-owned buffer
    /// has completed.
    pub fn remove_task(&self, task: TaskId) -> binder::Result<TaskWindowSpec> {
        let _gate = lock(&self.gate)?;
        let spec = lock(&self.session)?
            .windows_mut()
            .remove(task)
            .map_err(|_| service_error(ErrorCode::BadDisplay))?;
        let was_connected = lock(&self.connected)?.remove(&spec.display);
        self.vsync.disable(spec.display);
        self.modes.forget(spec.display);
        if was_connected {
            let display =
                i64::try_from(spec.display.0).map_err(|_| service_error(ErrorCode::BadDisplay))?;
            if let Some(callback) = lock(&self.callback)?.as_ref() {
                callback.onHotplugEvent(display, DisplayHotplugEvent::DISCONNECTED)?;
            }
        }
        Ok(spec)
    }

    /// Disconnect and remove a staged display, including a launch that failed
    /// before Android returned a task ID.
    pub fn remove_display(&self, display: DisplayId) -> binder::Result<Option<TaskWindowSpec>> {
        let _gate = lock(&self.gate)?;
        let was_connected = lock(&self.connected)?.remove(&display);
        self.vsync.disable(display);
        self.modes.forget(display);
        if was_connected {
            let aidl_display =
                i64::try_from(display.0).map_err(|_| service_error(ErrorCode::BadDisplay))?;
            if let Some(callback) = lock(&self.callback)?.as_ref() {
                callback.onHotplugEvent(aidl_display, DisplayHotplugEvent::DISCONNECTED)?;
            }
        }
        lock(&self.session)?
            .windows_mut()
            .remove_display(display)
            .map_err(|_| service_error(ErrorCode::BadDisplay))
    }

    /// Request composition for a connected task after Denial state changes.
    pub fn refresh(&self, display: DisplayId) -> binder::Result<()> {
        let _gate = lock(&self.gate)?;
        if !lock(&self.connected)?.contains(&display) {
            return Err(service_error(ErrorCode::BadDisplay));
        }
        let configure = lock(&self.session)?
            .windows()
            .composer_for_display(display)
            .map_err(|_| service_error(ErrorCode::BadDisplay))?
            .latest_configure()
            .ok_or_else(|| service_error(ErrorCode::BadConfig))?;
        self.vsync
            .update_period(display, self.modes.active_period(display, configure)?)?;
        let display = i64::try_from(display.0).map_err(|_| service_error(ErrorCode::BadDisplay))?;
        if let Some(callback) = lock(&self.callback)?.as_ref() {
            // SurfaceFlinger caches physical display modes. onRefresh alone
            // redraws layers without rereading the changed vsync period.
            // Reannounce this existing display so it reloads the mode while
            // preserving its identity; only changed host configures get here.
            callback.onHotplugEvent(display, DisplayHotplugEvent::CONNECTED)?;
            callback.onRefresh(display)?;
        }
        Ok(())
    }

    /// Return whether a task display has completed initial configure and been
    /// announced to `SurfaceFlinger`.
    pub fn is_connected(&self, display: DisplayId) -> binder::Result<bool> {
        let _gate = lock(&self.gate)?;
        Ok(lock(&self.connected)?.contains(&display))
    }

    /// Phase-lock synthetic HWC vsync to a real host presentation sample.
    /// Ignored when the host clock drives vsync.
    pub fn observe_presentation(&self, timestamp_nanos: u64, period_nanos: u64) {
        self.vsync
            .observe_presentation(timestamp_nanos, period_nanos);
    }

    /// Deliver one host frame grant as SurfaceFlinger's vsync.
    pub fn frame_grant(&self, latch_nanos: u64, period_nanos: u64) {
        self.vsync.frame_grant(latch_nanos, period_nanos);
    }
}

/// Generic frozen-V5 service. `F` creates one native-buffer table per Binder
/// client, preventing imported handles from leaking across client lifetimes.
pub struct ComposerService<A, F>
where
    A: NativeBufferAdapter + Send + 'static,
    F: Fn() -> A + Send + Sync + 'static,
{
    session: SharedSession,
    buffer_factory: F,
    callback: SharedCallback,
    connected: ConnectedDisplays,
    lifecycle_gate: Arc<Mutex<()>>,
    vsync: VsyncScheduler,
    modes: SharedModes,
    client_active: Arc<AtomicBool>,
    marker: PhantomData<fn() -> A>,
}

impl<A, F> ComposerService<A, F>
where
    A: NativeBufferAdapter + Send + 'static,
    F: Fn() -> A + Send + Sync + 'static,
{
    /// Create an unregistered service around the per-task Composer session.
    /// With `host_clock`, vsync comes only from forwarded host frame grants.
    pub fn new(session: SharedSession, buffer_factory: F, host_clock: bool) -> Self {
        let callback = Arc::new(Mutex::new(None));
        let vsync = VsyncScheduler::new(Arc::clone(&callback), host_clock);
        Self {
            session,
            buffer_factory,
            callback,
            connected: Arc::new(Mutex::new(BTreeSet::new())),
            lifecycle_gate: Arc::new(Mutex::new(())),
            vsync,
            modes: SharedModes::default(),
            client_active: Arc::new(AtomicBool::new(false)),
            marker: PhantomData,
        }
    }

    /// Hear the refresh rate SurfaceFlinger chooses for each task display.
    pub fn set_frame_rate_listener(&self, listener: FrameRateListener) {
        if let Ok(mut slot) = self.modes.listener.lock() {
            *slot = Some(listener);
        }
    }

    /// Return a cloneable task-display staging/hotplug handle before wrapping
    /// the service as a Binder object.
    pub fn lifecycle(&self) -> ComposerLifecycle {
        ComposerLifecycle {
            session: Arc::clone(&self.session),
            callback: Arc::clone(&self.callback),
            connected: Arc::clone(&self.connected),
            gate: Arc::clone(&self.lifecycle_gate),
            vsync: self.vsync.clone(),
            modes: Arc::clone(&self.modes),
        }
    }

    /// Wrap this implementation as a VINTF-stable Binder object.
    ///
    /// This does not register the service with servicemanager.
    pub fn into_binder(self) -> Strong<dyn IComposer> {
        BnComposer::new_binder(self, BinderFeatures::default())
    }
}

impl<A, F> Interface for ComposerService<A, F>
where
    A: NativeBufferAdapter + Send + 'static,
    F: Fn() -> A + Send + Sync + 'static,
{
}

impl<A, F> IComposer for ComposerService<A, F>
where
    A: NativeBufferAdapter + Send + 'static,
    F: Fn() -> A + Send + Sync + 'static,
{
    fn createClient(&self) -> binder::Result<Strong<dyn IComposerClient>> {
        self.client_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| service_error(ErrorCode::NoResources))?;

        let client = ComposerClient {
            session: Arc::clone(&self.session),
            buffers: Mutex::new((self.buffer_factory)()),
            callback: Arc::clone(&self.callback),
            connected: Arc::clone(&self.connected),
            lifecycle_gate: Arc::clone(&self.lifecycle_gate),
            vsync: self.vsync.clone(),
            modes: Arc::clone(&self.modes),
            lease: ClientLease(Arc::clone(&self.client_active)),
        };
        Ok(BnComposerClient::new_binder(
            client,
            BinderFeatures::default(),
        ))
    }

    fn getCapabilities(&self) -> binder::Result<Vec<Capability>> {
        // The fence returned by the direct SurfaceFlinger path represents GPU
        // completion so Android can safely release the source layers. Denial's
        // actual host presentation arrives asynchronously through Wayland and
        // is not represented by that fence. Tell SurfaceFlinger not to train
        // its vsync predictor from GPU-completion timestamps; it must keep the
        // phase-stable HWC-vsync source enabled instead.
        Ok(vec![Capability::PRESENT_FENCE_IS_NOT_RELIABLE])
    }
}

struct ClientLease(Arc<AtomicBool>);

impl Drop for ClientLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// One Surface Flinger Composer client over independent Android task windows.
pub struct ComposerClient<A>
where
    A: NativeBufferAdapter + Send + 'static,
{
    session: SharedSession,
    buffers: Mutex<A>,
    callback: SharedCallback,
    connected: ConnectedDisplays,
    lifecycle_gate: Arc<Mutex<()>>,
    vsync: VsyncScheduler,
    modes: SharedModes,
    lease: ClientLease,
}

impl<A> Drop for ComposerClient<A>
where
    A: NativeBufferAdapter + Send + 'static,
{
    fn drop(&mut self) {
        if let Ok(mut callback) = self.callback.lock() {
            callback.take();
        }
        self.vsync.clear();
        // `lease` is dropped immediately after this method.
        let _ = &self.lease;
    }
}

impl<A> ComposerClient<A>
where
    A: NativeBufferAdapter + Send + 'static,
{
    /// Apply SurfaceFlinger's mode choice and tell the host its rate.
    fn switch_config(&self, display: i64, config: i32) -> binder::Result<()> {
        let configure = configured_display(&self.session, display)?;
        let period = DisplayModes::config_period(configure, config)?;
        let display = display_id(display)?;
        if self.modes.set_active(display, config)? {
            self.vsync.update_period(display, period)?;
        }
        let millihz = if config == LOW_CONFIG_ID {
            LOW_REFRESH_MILLIHZ
        } else {
            configure.refresh_millihz
        };
        self.modes.announce(display, millihz);
        Ok(())
    }
}

impl<A> Interface for ComposerClient<A> where A: NativeBufferAdapter + Send + 'static {}

#[allow(deprecated)]
impl<A> IComposerClient for ComposerClient<A>
where
    A: NativeBufferAdapter + Send + 'static,
{
    fn createLayer(&self, display: i64, buffer_slot_count: i32) -> binder::Result<i64> {
        let display = display_id(display)?;
        let buffer_slot_count =
            u32::try_from(buffer_slot_count).map_err(|_| service_error(ErrorCode::BadParameter))?;
        let mut session = lock(&self.session)?;
        let layer = session
            .create_layer(display, buffer_slot_count)
            .map_err(service_error)?;
        let mut buffers = match lock(&self.buffers) {
            Ok(buffers) => buffers,
            Err(error) => {
                let _ = session.destroy_layer(display, layer);
                return Err(error);
            }
        };
        if let Err(code) = buffers.create_layer_slots(display, layer, buffer_slot_count) {
            let _ = session.destroy_layer(display, layer);
            return Err(service_error(code));
        }
        match i64::try_from(layer.0) {
            Ok(layer) => Ok(layer),
            Err(_) => {
                buffers.destroy_layer_slots(display, layer);
                let _ = session.destroy_layer(display, layer);
                Err(service_error(ErrorCode::NoResources))
            }
        }
    }

    fn createVirtualDisplay(
        &self,
        _width: i32,
        _height: i32,
        _format_hint: PixelFormat,
        _output_buffer_slot_count: i32,
    ) -> binder::Result<VirtualDisplay> {
        unsupported()
    }

    fn destroyLayer(&self, display: i64, layer: i64) -> binder::Result<()> {
        let display = display_id(display)?;
        let layer = layer_id(layer)?;
        lock(&self.session)?
            .destroy_layer(display, layer)
            .map_err(service_error)?;
        lock(&self.buffers)?.destroy_layer_slots(display, layer);
        Ok(())
    }

    fn destroyVirtualDisplay(&self, _display: i64) -> binder::Result<()> {
        unsupported()
    }

    fn executeCommands(
        &self,
        commands: &[DisplayCommand],
    ) -> binder::Result<Vec<CommandResultPayload>> {
        let mut session = lock(&self.session)?;
        let mut buffers = lock(&self.buffers)?;
        Ok(execute_commands(&mut session, commands, &mut *buffers))
    }

    fn getActiveConfig(&self, display: i64) -> binder::Result<i32> {
        let configure = configured_display(&self.session, display)?;
        self.modes.active_config(display_id(display)?, configure)
    }

    fn getColorModes(&self, display: i64) -> binder::Result<Vec<ColorMode>> {
        require_display(&self.session, display)?;
        Ok(vec![ColorMode::NATIVE])
    }

    fn getDataspaceSaturationMatrix(&self, _dataspace: Dataspace) -> binder::Result<Vec<f32>> {
        unsupported()
    }

    fn getDisplayAttribute(
        &self,
        display: i64,
        config: i32,
        attribute: DisplayAttribute,
    ) -> binder::Result<i32> {
        let configure = configured_display(&self.session, display)?;
        let period = DisplayModes::config_period(configure, config)?;
        match attribute {
            DisplayAttribute::WIDTH => {
                i32::try_from(configure.width).map_err(|_| service_error(ErrorCode::BadConfig))
            }
            DisplayAttribute::HEIGHT => {
                i32::try_from(configure.height).map_err(|_| service_error(ErrorCode::BadConfig))
            }
            DisplayAttribute::VSYNC_PERIOD => {
                i32::try_from(period).map_err(|_| service_error(ErrorCode::BadConfig))
            }
            DisplayAttribute::DPI_X | DisplayAttribute::DPI_Y => Ok(UNKNOWN_DPI_MILLI),
            DisplayAttribute::CONFIG_GROUP => Ok(CONFIG_GROUP),
            _ => Err(service_error(ErrorCode::BadParameter)),
        }
    }

    fn getDisplayCapabilities(&self, display: i64) -> binder::Result<Vec<DisplayCapability>> {
        require_display(&self.session, display)?;
        Ok(Vec::new())
    }

    fn getDisplayConfigs(&self, display: i64) -> binder::Result<Vec<i32>> {
        let configure = configured_display(&self.session, display)?;
        Ok(DisplayModes::configs(configure)?
            .into_iter()
            .map(|(id, _)| id)
            .collect())
    }

    fn getDisplayConnectionType(&self, display: i64) -> binder::Result<DisplayConnectionType> {
        require_display(&self.session, display)?;
        // Droidloom displays are private app-hosting surfaces, not physical
        // connectors. Report them as internal so Android admits every task
        // display directly instead of applying external-display policy.
        Ok(DisplayConnectionType::INTERNAL)
    }

    fn getDisplayIdentificationData(&self, display: i64) -> binder::Result<DisplayIdentification> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn getDisplayName(&self, display: i64) -> binder::Result<String> {
        let display = display_id(display)?;
        let session = lock(&self.session)?;
        let reservation = session
            .windows()
            .reservation_for_display(display)
            .map_err(|_| service_error(ErrorCode::BadDisplay))?;
        // DisplayManager allocates its own logical display ID after the HWC
        // hotplug. Include our stable Composer handle so the privileged task
        // launcher can resolve that asynchronous Android identity without
        // relying on allocation order or package-name uniqueness.
        Ok(format!("Droidloom:{}:{}", display.0, reservation.package))
    }

    fn getDisplayVsyncPeriod(&self, display: i64) -> binder::Result<i32> {
        let configure = configured_display(&self.session, display)?;
        i32::try_from(self.modes.active_period(display_id(display)?, configure)?)
            .map_err(|_| service_error(ErrorCode::BadConfig))
    }

    fn getDisplayedContentSample(
        &self,
        display: i64,
        _max_frames: i64,
        _timestamp: i64,
    ) -> binder::Result<DisplayContentSample> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn getDisplayedContentSamplingAttributes(
        &self,
        display: i64,
    ) -> binder::Result<DisplayContentSamplingAttributes> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn getDisplayPhysicalOrientation(&self, display: i64) -> binder::Result<Transform> {
        require_display(&self.session, display)?;
        Ok(Transform::NONE)
    }

    fn getHdrCapabilities(&self, display: i64) -> binder::Result<HdrCapabilities> {
        require_display(&self.session, display)?;
        Ok(HdrCapabilities::default())
    }

    fn getMaxVirtualDisplayCount(&self) -> binder::Result<i32> {
        Ok(0)
    }

    fn getPerFrameMetadataKeys(&self, display: i64) -> binder::Result<Vec<PerFrameMetadataKey>> {
        require_display(&self.session, display)?;
        Ok(Vec::new())
    }

    fn getReadbackBufferAttributes(
        &self,
        display: i64,
    ) -> binder::Result<ReadbackBufferAttributes> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn getReadbackBufferFence(
        &self,
        display: i64,
    ) -> binder::Result<Option<binder::ParcelFileDescriptor>> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn getRenderIntents(&self, display: i64, mode: ColorMode) -> binder::Result<Vec<RenderIntent>> {
        require_display(&self.session, display)?;
        if mode != ColorMode::NATIVE {
            return Err(service_error(ErrorCode::BadParameter));
        }
        Ok(vec![RenderIntent::COLORIMETRIC])
    }

    fn getSupportedContentTypes(&self, display: i64) -> binder::Result<Vec<ContentType>> {
        require_display(&self.session, display)?;
        Ok(Vec::new())
    }

    fn getDisplayDecorationSupport(
        &self,
        display: i64,
    ) -> binder::Result<Option<DisplayDecorationSupport>> {
        require_display(&self.session, display)?;
        Ok(None)
    }

    fn registerCallback(&self, callback: &Strong<dyn IComposerCallback>) -> binder::Result<()> {
        let _gate = lock(&self.lifecycle_gate)?;
        let displays = {
            let connected = lock(&self.connected)?;
            connected
                .iter()
                .map(|display| {
                    i64::try_from(display.0).map_err(|_| service_error(ErrorCode::BadDisplay))
                })
                .collect::<binder::Result<Vec<_>>>()?
        };

        *lock(&self.callback)? = Some(callback.clone());
        for display in displays {
            callback.onHotplugEvent(display, DisplayHotplugEvent::CONNECTED)?;
        }
        Ok(())
    }

    fn setActiveConfig(&self, display: i64, config: i32) -> binder::Result<()> {
        self.switch_config(display, config)
    }

    fn setActiveConfigWithConstraints(
        &self,
        display: i64,
        config: i32,
        _constraints: &VsyncPeriodChangeConstraints,
    ) -> binder::Result<VsyncPeriodChangeTimeline> {
        self.switch_config(display, config)?;
        // Applied at once: vsync follows the host's grants, and the host
        // changes its own rate after hearing the new one.
        Ok(VsyncPeriodChangeTimeline {
            newVsyncAppliedTimeNanos: i64::try_from(monotonic_timestamp_nanos()?)
                .map_err(|_| service_error(ErrorCode::BadConfig))?,
            refreshRequired: false,
            refreshTimeNanos: 0,
        })
    }

    fn setBootDisplayConfig(&self, display: i64, _config: i32) -> binder::Result<()> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn clearBootDisplayConfig(&self, display: i64) -> binder::Result<()> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn getPreferredBootDisplayConfig(&self, display: i64) -> binder::Result<i32> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn setAutoLowLatencyMode(&self, display: i64, _on: bool) -> binder::Result<()> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn setClientTargetSlotCount(&self, display: i64, count: i32) -> binder::Result<()> {
        let display = require_display(&self.session, display)?;
        let count = u32::try_from(count).map_err(|_| service_error(ErrorCode::BadParameter))?;
        if count == 0 || count > MAX_BUFFER_SLOTS {
            return Err(service_error(ErrorCode::BadParameter));
        }
        lock(&self.buffers)?
            .set_client_target_slot_count(display, count)
            .map_err(service_error)
    }

    fn setColorMode(
        &self,
        display: i64,
        mode: ColorMode,
        intent: RenderIntent,
    ) -> binder::Result<()> {
        require_display(&self.session, display)?;
        if mode == ColorMode::NATIVE && intent == RenderIntent::COLORIMETRIC {
            Ok(())
        } else {
            Err(service_error(ErrorCode::BadParameter))
        }
    }

    fn setContentType(&self, display: i64, _content_type: ContentType) -> binder::Result<()> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn setDisplayedContentSamplingEnabled(
        &self,
        display: i64,
        _enable: bool,
        _component_mask: FormatColorComponent,
        _max_frames: i64,
    ) -> binder::Result<()> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn setPowerMode(&self, display: i64, mode: AidlPowerMode) -> binder::Result<()> {
        let mode = match mode {
            AidlPowerMode::OFF => PowerMode::Off,
            AidlPowerMode::ON => PowerMode::On,
            AidlPowerMode::DOZE => PowerMode::Doze,
            AidlPowerMode::DOZE_SUSPEND => PowerMode::DozeSuspend,
            _ => return Err(service_error(ErrorCode::Unsupported)),
        };
        let display = display_id(display)?;
        lock(&self.session)?
            .set_power_mode(display, mode)
            .map_err(service_error)?;
        // Powering off also ends vsync; SF enables it again after power-on.
        if mode == PowerMode::Off {
            self.vsync.disable(display);
        }
        Ok(())
    }

    fn setReadbackBuffer(
        &self,
        display: i64,
        _buffer: &NativeHandle,
        _release_fence: Option<&binder::ParcelFileDescriptor>,
    ) -> binder::Result<()> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn setVsyncEnabled(&self, display: i64, enabled: bool) -> binder::Result<()> {
        let display = display_id(display)?;
        let period_nanos = {
            let mut session = lock(&self.session)?;
            session
                .set_vsync_enabled(display, enabled)
                .map_err(service_error)?;
            let configure = session
                .windows()
                .composer_for_display(display)
                .map_err(|_| service_error(ErrorCode::BadDisplay))?
                .latest_configure()
                .ok_or_else(|| service_error(ErrorCode::BadConfig))?;
            self.modes.active_period(display, configure)?
        };
        self.vsync.set_enabled(display, enabled, period_nanos)
    }

    fn setIdleTimerEnabled(&self, display: i64, _timeout_ms: i32) -> binder::Result<()> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn getOverlaySupport(&self) -> binder::Result<OverlayProperties> {
        Ok(OverlayProperties::default())
    }

    fn getHdrConversionCapabilities(&self) -> binder::Result<Vec<HdrConversionCapability>> {
        Ok(Vec::new())
    }

    fn setHdrConversionStrategy(&self, _strategy: &HdrConversionStrategy) -> binder::Result<Hdr> {
        unsupported()
    }

    fn setRefreshRateChangedCallbackDebugEnabled(
        &self,
        display: i64,
        _enabled: bool,
    ) -> binder::Result<()> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn getDisplayConfigurations(
        &self,
        display: i64,
        _max_frame_interval_ns: i32,
    ) -> binder::Result<Vec<DisplayConfiguration>> {
        let configure = configured_display(&self.session, display)?;
        let width =
            i32::try_from(configure.width).map_err(|_| service_error(ErrorCode::BadConfig))?;
        let height =
            i32::try_from(configure.height).map_err(|_| service_error(ErrorCode::BadConfig))?;
        DisplayModes::configs(configure)?
            .into_iter()
            .map(|(config_id, period)| {
                Ok(DisplayConfiguration {
                    configId: config_id,
                    width,
                    height,
                    dpi: None,
                    configGroup: CONFIG_GROUP,
                    vsyncPeriod: i32::try_from(period)
                        .map_err(|_| service_error(ErrorCode::BadConfig))?,
                    vrrConfig: None,
                    hdrOutputType: OutputType::SDR,
                })
            })
            .collect()
    }

    fn notifyExpectedPresent(
        &self,
        display: i64,
        _expected_present_time: &ClockMonotonicTimestamp,
        _frame_interval_ns: i32,
    ) -> binder::Result<()> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn getMaxLayerPictureProfiles(&self, display: i64) -> binder::Result<i32> {
        require_display(&self.session, display)?;
        Ok(0)
    }

    fn startHdcpNegotiation(&self, display: i64, _levels: &HdcpLevels) -> binder::Result<()> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn getLuts(&self, display: i64, _buffers: &[Buffer]) -> binder::Result<Vec<Luts>> {
        require_display(&self.session, display)?;
        unsupported()
    }

    fn getDisplayKnownVsyncSample(&self, display: i64) -> binder::Result<VsyncSample> {
        require_display(&self.session, display)?;
        unsupported()
    }
}

fn display_id(value: i64) -> binder::Result<DisplayId> {
    u64::try_from(value)
        .map(DisplayId)
        .map_err(|_| service_error(ErrorCode::BadDisplay))
}

fn layer_id(value: i64) -> binder::Result<LayerId> {
    u64::try_from(value)
        .map(LayerId)
        .map_err(|_| service_error(ErrorCode::BadLayer))
}

fn require_display(session: &SharedSession, value: i64) -> binder::Result<DisplayId> {
    let display = display_id(value)?;
    lock(session)?
        .windows()
        .composer_for_display(display)
        .map_err(|_| service_error(ErrorCode::BadDisplay))?;
    Ok(display)
}

fn configured_display(session: &SharedSession, value: i64) -> binder::Result<Configure> {
    let display = display_id(value)?;
    lock(session)?
        .windows()
        .composer_for_display(display)
        .map_err(|_| service_error(ErrorCode::BadDisplay))?
        .latest_configure()
        .ok_or_else(|| service_error(ErrorCode::BadConfig))
}

fn configure_refresh_period(configure: Configure) -> binder::Result<u64> {
    if configure.refresh_millihz == 0 {
        return Err(service_error(ErrorCode::BadConfig));
    }
    Ok(1_000_000_000_000_u64 / u64::from(configure.refresh_millihz))
}

#[cfg(test)]
mod tests {
    use super::{next_vsync_after, periods_compatible, phase_distance, ScheduledVsync};

    #[test]
    fn period_change_keeps_the_host_phase() {
        let started = ScheduledVsync::start(1_000, 10, Some((83, 10)));
        assert_eq!((started.next_nanos, started.phased), (1_003, true));

        let mut changed = ScheduledVsync::start(1_000, 20, Some((83, 10)));
        assert_eq!((changed.next_nanos, changed.phased), (1_020, false));
        changed.observe(1_005, 997, 10);
        assert_eq!((changed.next_nanos, changed.phased), (1_020, false));
        changed.observe(1_005, 991, 20);
        assert_eq!((changed.next_nanos, changed.phased), (1_011, true));
        changed.observe(1_006, 1_002, 20);
        assert_eq!(changed.next_nanos, 1_011);
    }

    #[test]
    fn predicted_vsync_advances_from_the_host_phase_without_drift() {
        assert_eq!(next_vsync_after(100, 80, 10), 110);
        assert_eq!(next_vsync_after(100, 105, 10), 105);
        assert_eq!(next_vsync_after(107, 80, 10), 110);
    }

    #[test]
    fn phase_correction_requires_matching_refresh_periods() {
        assert!(periods_compatible(8_333_333, 8_333_334));
        assert!(!periods_compatible(8_333_333, 16_666_666));
    }

    #[test]
    fn phase_distance_ignores_whole_refresh_periods() {
        assert_eq!(phase_distance(110, 80, 10), 0);
        assert_eq!(phase_distance(107, 80, 10), 3);
        assert_eq!(phase_distance(108, 80, 10), 2);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> binder::Result<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| service_error(ErrorCode::NoResources))
}

fn unsupported<T>() -> binder::Result<T> {
    Err(service_error(ErrorCode::Unsupported))
}

fn service_error(code: ErrorCode) -> binder::Status {
    binder::Status::new_service_specific_error(code as i32, None)
}
