//! Optional compositor grants. Once selected, exact mode never invents a clock.
use super::{
    AcceptedWireFrame, App, Connection, Dispatch, FrameFeedback, FrameId, OwnedFd,
    PresentationTimeState, PresenterError, Proxy, QueueHandle, SceneFeedback, TaskObjectId,
    monotonic_timestamp_nanos, presentation_timestamp_nanos, readiness, split_point, wl_output,
    wl_surface,
};
use smithay_client_toolkit::globals::GlobalData;
use smithay_client_toolkit::registry::RegistryHandler;
use std::collections::{BTreeSet, VecDeque};
use wayland_protocols::wp::presentation_time::client::wp_presentation::{self, WpPresentation};

mod protocol {
    #![allow(
        dead_code,
        non_camel_case_types,
        non_upper_case_globals,
        non_snake_case,
        unused_imports,
        unused_unsafe,
        unused_variables,
        clippy::all,
        clippy::pedantic,
        missing_docs
    )]
    use wayland_client;
    use wayland_client::protocol::*;
    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../protocol/denial-frame-timeline-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("../../protocol/denial-frame-timeline-v1.xml");
}
pub(super) use protocol::denial_frame_timeline_manager_v1::DenialFrameTimelineManagerV1 as Manager;
pub(super) use protocol::denial_output_frame_timeline_v1::{
    self as output_timeline, DenialOutputFrameTimelineV1 as OutputTimeline,
};
use protocol::denial_surface_frame_timeline_v1::DenialSurfaceFrameTimelineV1;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Identity {
    epoch: u64,
    sequence: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Grant {
    identity: Identity,
    pub latch_deadline_ns: u64,
    pub presentation_target_ns: u64,
    pub refresh_period_ns: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Terminal {
    Presented,
    Skipped,
    Missed,
    Configure,
    Epoch,
}

#[derive(Default)]
struct Schedule {
    epoch: Option<u64>,
    last_sequence: u64,
    offered: Option<Grant>,
    consumed: BTreeSet<Identity>,
    // Bounded lifetime counters, independent of GPU reads and buffer ownership.
    terminal: [u64; 5],
}
impl Schedule {
    fn count(&mut self, terminal: Terminal) {
        let count = &mut self.terminal[terminal as usize];
        *count = count.saturating_add(1);
    }
    fn finish(&mut self, identity: Identity, terminal: Terminal) {
        if self.consumed.remove(&identity) {
            self.count(terminal);
        }
    }
    fn clear(&mut self, terminal: Terminal) {
        if self.offered.take().is_some() {
            self.count(terminal);
        }
        while self.consumed.pop_first().is_some() {
            self.count(terminal);
        }
    }
    fn epoch(&mut self, epoch: u64) {
        if self.epoch != Some(epoch) {
            self.clear(Terminal::Epoch);
            self.epoch = Some(epoch);
            self.last_sequence = 0;
        }
    }
    fn offer(&mut self, grant: Grant, now: u64) {
        if self.epoch != Some(grant.identity.epoch)
            || grant.identity.sequence <= self.last_sequence
            || grant.refresh_period_ns == 0
            || grant.latch_deadline_ns >= grant.presentation_target_ns
        {
            return;
        }
        if let Some(previous) = self.offered.take() {
            self.count(if now >= previous.latch_deadline_ns {
                Terminal::Missed
            } else {
                Terminal::Skipped
            });
        }
        self.last_sequence = grant.identity.sequence;
        self.offered = Some(grant);
    }
    fn consume(&mut self, now: u64) -> Option<Grant> {
        let grant = self.offered.take()?;
        if now >= grant.latch_deadline_ns {
            self.count(Terminal::Missed);
            return None;
        }
        self.consumed.insert(grant.identity);
        Some(grant)
    }
}

pub(super) struct Surface(DenialSurfaceFrameTimelineV1);
impl Surface {
    pub fn new<D: Dispatch<DenialSurfaceFrameTimelineV1, ()> + 'static>(
        manager: &Manager,
        surface: &wl_surface::WlSurface,
        qh: &QueueHandle<D>,
    ) -> Self {
        Self(manager.get_surface_timeline(surface, qh, ()))
    }
    pub fn target(&self, target: &Target) {
        let (epoch_hi, epoch_lo) = split_point(target.identity.epoch);
        let (sequence_hi, sequence_lo) = split_point(target.identity.sequence);
        self.0.set_target(
            &target.timeline,
            epoch_hi,
            epoch_lo,
            sequence_hi,
            sequence_lo,
        );
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

pub(super) struct Target {
    timeline: OutputTimeline,
    pub identity: Identity,
    pub latch_ns: u64,
}

pub(super) struct Reservation {
    pub target: Option<Target>,
}

// Armed only while Android draws; see clock.rs.
pub(super) struct Subscription {
    pub output: wl_output::WlOutput,
    proxy: OutputTimeline,
    armed: bool,
}
impl Subscription {
    pub fn new(output: wl_output::WlOutput, proxy: OutputTimeline, armed: bool) -> Self {
        if armed {
            proxy.set_active(1);
        }
        Self {
            output,
            proxy,
            armed,
        }
    }
    pub fn arm(&mut self, armed: bool) {
        if self.armed != armed {
            self.proxy.set_active(u32::from(armed));
            self.armed = armed;
        }
    }
    pub fn is(&self, proxy: &OutputTimeline) -> bool {
        self.proxy == *proxy
    }
    fn report_content_rate(&self, millihz: u32) {
        if self.proxy.version() >= 2 {
            self.proxy.content_rate(millihz);
        }
    }
}

/// Android content faster than this asks Denial for its high refresh rate.
/// Matches Denial's own threshold for fast content.
const FAST_CONTENT_MILLIHZ: u64 = 80_000;
/// Frames in a row whose rate counts.
const RATE_FRAMES: usize = 6;
/// Android frames that had to wait for a grant, in a row, and the rate
/// above which they mean Android draws faster than the output.
const PARK_FRAMES: usize = 4;
const FAST_PARK_MILLIHZ: u64 = 20_000;
/// How often a fast rate is repeated; Denial holds it for 300 ms.
const RATE_REPORT_NANOS: u64 = 100_000_000;

/// Times of the last few events of one kind and the rate they ran at.
struct EventRate<const N: usize> {
    times: VecDeque<u64>,
}
impl<const N: usize> Default for EventRate<N> {
    fn default() -> Self {
        Self {
            times: VecDeque::with_capacity(N),
        }
    }
}
impl<const N: usize> EventRate<N> {
    fn note(&mut self, now: u64) -> Option<u64> {
        if self.times.len() == N {
            self.times.pop_front();
        }
        self.times.push_back(now);
        if self.times.len() < N {
            return None;
        }
        let span = now.saturating_sub(*self.times.front()?);
        (span > 0).then(|| (N as u64 - 1) * 1_000_000_000_000 / span)
    }
}

/// Android draws on its own vsync, and Denial sees at most one of its frames
/// per grant, so at the low refresh rate fast Android content never shows in
/// Denial's own measurement. The task reports it instead, from two signals:
/// - frames shown faster than `FAST_CONTENT_MILLIHZ` (the output already
///   runs fast and the content keeps up with it);
/// - frames that keep arriving before their grant: SurfaceFlinger composes
///   ahead of every grant only when its content is faster than the output.
#[derive(Default)]
struct RateMeter {
    frames: EventRate<RATE_FRAMES>,
    parks: EventRate<PARK_FRAMES>,
    /// The frame waiting now was counted; it is retried on every grant
    /// and release until shown.
    waiting: bool,
    last_report: Option<u64>,
}
impl RateMeter {
    fn report(&mut self, now: u64, millihz: u64) -> Option<u32> {
        if self
            .last_report
            .is_some_and(|last| now.saturating_sub(last) < RATE_REPORT_NANOS)
        {
            return None;
        }
        self.last_report = Some(now);
        Some(u32::try_from(millihz).unwrap_or(u32::MAX))
    }
    /// One Android frame was shown; returns the rate to report, if any.
    fn frame(&mut self, now: u64) -> Option<u32> {
        self.waiting = false;
        let millihz = self.frames.note(now)?;
        (millihz >= FAST_CONTENT_MILLIHZ)
            .then(|| self.report(now, millihz))
            .flatten()
    }
    /// One Android frame waits for a grant; returns the rate to report.
    fn park(&mut self, now: u64) -> Option<u32> {
        if std::mem::replace(&mut self.waiting, true) {
            return None;
        }
        let millihz = self.parks.note(now)?;
        (millihz >= FAST_PARK_MILLIHZ)
            .then(|| self.report(now, FAST_CONTENT_MILLIHZ))
            .flatten()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if self.armed {
            self.proxy.set_active(0);
        }
        self.proxy.destroy();
    }
}

pub(super) struct QueuedFrame {
    pub frame: AcceptedWireFrame,
    pub fence: OwnedFd,
}

#[derive(Default)]
pub(super) struct Timeline {
    pub exact: bool,
    pub surface: Option<Surface>,
    subscription: Option<Subscription>,
    schedule: Schedule,
    pub queued: Option<QueuedFrame>,
    // Frames that reached an offered grant after its latch.
    pub late: u64,
    rate: RateMeter,
}
impl Timeline {
    pub fn new<D: Dispatch<DenialSurfaceFrameTimelineV1, ()> + 'static>(
        manager: Option<&Manager>,
        surface: Option<&wl_surface::WlSurface>,
        qh: &QueueHandle<D>,
    ) -> Self {
        Self {
            exact: manager.is_some() && surface.is_some(),
            surface: manager.zip(surface).map(|(m, s)| Surface::new(m, s, qh)),
            ..Self::default()
        }
    }
    pub fn finish(&mut self, identity: Option<Identity>, terminal: Terminal) {
        if let Some(identity) = identity {
            self.schedule.finish(identity, terminal);
        }
    }
    pub fn reserve(&mut self, now: u64) -> Option<Reservation> {
        if !self.exact {
            return Some(Reservation { target: None });
        }
        let subscription = self.subscription.as_ref()?;
        // Disarmed means Android was idle: show this frame at once, untargeted,
        // while the caller arms grants for the frames after it.
        if !subscription.armed {
            return Some(Reservation { target: None });
        }
        let late = self
            .schedule
            .offered
            .is_some_and(|grant| now >= grant.latch_deadline_ns);
        let Some(grant) = self.schedule.consume(now) else {
            self.late += u64::from(late);
            return None;
        };
        Some(Reservation {
            target: Some(Target {
                timeline: subscription.proxy.clone(),
                identity: grant.identity,
                latch_ns: grant.latch_deadline_ns,
            }),
        })
    }
    /// Counts one Android frame shown and reports fast content to Denial.
    pub fn note_frame(&mut self, now: u64) {
        let report = self.rate.frame(now);
        self.report(report);
    }
    /// Counts one Android frame that waits for a grant.
    pub fn note_park(&mut self, now: u64) {
        let report = self.rate.park(now);
        self.report(report);
    }
    fn report(&self, millihz: Option<u32>) {
        if let Some(millihz) = millihz
            && let Some(subscription) = self.subscription.as_ref()
        {
            subscription.report_content_rate(millihz);
        }
    }
    /// Arm or disarm grant delivery. A grant offered but not taken is dropped.
    pub fn arm(&mut self, armed: bool) {
        if let Some(subscription) = self.subscription.as_mut() {
            subscription.arm(armed);
        }
        if !armed && self.schedule.offered.take().is_some() {
            self.schedule.count(Terminal::Skipped);
        }
    }
    pub fn stop(&mut self) {
        self.subscription = None;
        self.surface = None;
        self.schedule.clear(Terminal::Epoch);
    }
    /// Grant state for stall reports. Terminal counts are in `Terminal` order:
    /// presented, skipped, missed, configure, epoch.
    pub fn describe(&self) -> String {
        format!(
            "exact={} subscribed={} epoch={:?} last grant={} offered={} consumed={} terminals={:?}",
            self.exact,
            self.subscription.is_some(),
            self.schedule.epoch,
            self.schedule.last_sequence,
            self.schedule.offered.is_some(),
            self.schedule.consumed.len(),
            self.schedule.terminal,
        )
    }
}

impl App {
    pub(super) fn queue_present(
        &mut self,
        qh: &QueueHandle<Self>,
        frame: AcceptedWireFrame,
        fence: OwnedFd,
    ) -> Result<(), PresenterError> {
        let object = frame.object;
        let task = self
            .tasks
            .get_mut(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        if let Ok(now) = monotonic_timestamp_nanos() {
            task.timeline.note_frame(now);
        }
        if !task.timeline.exact || !task.accepts_present() {
            return self.present(qh, &frame, &fence, None);
        }
        // Preserve fallback/direct ordering across the two Android sockets.
        if let Some(stream) = task.layers.as_mut() {
            stream.abort_timeline_wait()?;
        }
        if let Some(previous) = task.timeline.queued.replace(QueuedFrame { frame, fence }) {
            self.endpoint
                .as_mut()
                .ok_or(PresenterError::Configuration("endpoint disappeared"))?
                .discard_frame(object, previous.frame.frame)?;
        }
        self.task_work.mark(object, readiness::TIMELINE);
        Ok(())
    }

    // Also used for configure, output changes, epoch changes and toplevel teardown.
    // Optional callbacks retain no producer read or release fence.
    pub(super) fn retire_feedback(&mut self, object: TaskObjectId, terminal: Terminal) {
        let Some(task) = self.tasks.get_mut(&object) else {
            return;
        };
        retire_feedback(
            &mut task.timeline,
            &mut task.scene_feedback,
            &mut task.presentation_feedback,
            terminal,
            |frame| {
                if let Some(endpoint) = self.endpoint.as_mut() {
                    endpoint.forget_presentation(object, frame);
                }
            },
        );
    }

    pub(super) fn abort_timeline_work(
        &mut self,
        object: TaskObjectId,
        terminal: Terminal,
    ) -> Result<(), PresenterError> {
        self.retire_feedback(object, terminal);
        let Some(task) = self.tasks.get_mut(&object) else {
            return Ok(());
        };
        task.timeline.schedule.clear(terminal);
        if let Some(queued) = task.timeline.queued.take() {
            if let Some(endpoint) = self.endpoint.as_mut() {
                endpoint.discard_frame(object, queued.frame.frame)?;
            }
        }
        if let Some(stream) = task.layers.as_mut() {
            stream.abort_timeline_wait()?;
        }
        Ok(())
    }

    pub(super) fn refresh_frame_timelines(
        &mut self,
        qh: &QueueHandle<Self>,
    ) -> Result<(), PresenterError> {
        // Droidloom's private host protocol carries CLOCK_MONOTONIC timestamps.
        // Never compare grants from an unannounced or incompatible clock to it.
        if self.frame_timeline_manager.is_some()
            && self.presentation_clock != Some(libc::CLOCK_MONOTONIC as u32)
        {
            if self.presentation_clock.is_some() {
                return Err(PresenterError::Configuration(
                    "exact frame timeline requires CLOCK_MONOTONIC presentation clock",
                ));
            }
            return Ok(());
        }
        let outputs = self.output_state.outputs().collect::<Vec<_>>();
        let objects = self.tasks.keys().copied().collect::<Vec<_>>();
        for object in objects {
            let task = &self.tasks[&object];
            if !task.timeline.exact || !task.accepts_present() {
                continue;
            }
            let output = task
                .presentation_output
                .as_ref()
                .filter(|o| outputs.contains(o))
                .or_else(|| task.entered_outputs.iter().find(|o| outputs.contains(o)))
                .or_else(|| outputs.first())
                .cloned();
            if task.timeline.subscription.as_ref().map(|s| &s.output) == output.as_ref() {
                continue;
            }
            if task.timeline.subscription.is_some() || task.timeline.schedule.epoch.is_some() {
                self.abort_timeline_work(object, Terminal::Epoch)?;
            }
            let task = self.tasks.get_mut(&object).unwrap();
            task.timeline.subscription = None;
            task.timeline.schedule.epoch = None;
            task.timeline.schedule.last_sequence = 0;
            if let (Some(manager), Some(output)) = (&self.frame_timeline_manager, output) {
                let proxy = manager.get_timeline(&output, qh, object);
                task.timeline.subscription =
                    Some(Subscription::new(output, proxy, self.clock.armed()));
            }
        }
        self.refresh_clock(qh);
        Ok(())
    }

    pub(super) fn flush_timeline_frames(
        &mut self,
        qh: &QueueHandle<Self>,
    ) -> Result<(), PresenterError> {
        while let Some(object) = self.task_work.pop(readiness::TIMELINE) {
            let Some(task) = self.tasks.get_mut(&object) else {
                continue;
            };
            if task.timeline.queued.is_none() || task.presentation_feedback.len() >= 64 {
                continue;
            }
            let now = monotonic_timestamp_nanos()?;
            let Some(target) = task.timeline.reserve(now) else {
                continue;
            };
            let queued = task.timeline.queued.take().unwrap();
            self.android_frame(now, target.target.as_ref());
            self.present(qh, &queued.frame, &queued.fence, target.target)?;
        }
        Ok(())
    }
}

fn retire_feedback(
    timeline: &mut Timeline,
    scenes: &mut Vec<SceneFeedback>,
    frames: &mut Vec<FrameFeedback>,
    terminal: Terminal,
    mut forget: impl FnMut(FrameId),
) {
    for feedback in scenes.drain(..) {
        timeline.finish(feedback.grant, terminal);
    }
    for feedback in frames.drain(..) {
        timeline.finish(feedback.grant, terminal);
        forget(feedback.frame);
    }
}

impl Dispatch<OutputTimeline, TaskObjectId> for App {
    fn event(
        state: &mut Self,
        proxy: &OutputTimeline,
        event: output_timeline::Event,
        object: &TaskObjectId,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(task) = state.tasks.get(object) else {
            return;
        };
        if !task
            .timeline
            .subscription
            .as_ref()
            .is_some_and(|s| s.is(proxy))
        {
            return;
        }
        let old_epoch = task.timeline.schedule.epoch;
        let result = (|| {
            match event {
                output_timeline::Event::Epoch { epoch_hi, epoch_lo } => {
                    let epoch = join(epoch_hi, epoch_lo);
                    if old_epoch.is_some_and(|old| old != epoch) {
                        state.abort_timeline_work(*object, Terminal::Epoch)?;
                    }
                    state
                        .tasks
                        .get_mut(object)
                        .unwrap()
                        .timeline
                        .schedule
                        .epoch(epoch);
                }
                event @ output_timeline::Event::Grant { .. } => {
                    let grant = wire_grant(event)?;
                    let now = monotonic_timestamp_nanos()?;
                    state
                        .tasks
                        .get_mut(object)
                        .unwrap()
                        .timeline
                        .schedule
                        .offer(grant, now);
                    state
                        .task_work
                        .mark(*object, readiness::TIMELINE | readiness::RELEASES);
                    state.clock_idle_check(now);
                }
            }
            Ok::<(), PresenterError>(())
        })();
        if let Err(error) = result {
            state.fail(&error);
        }
    }
}

impl Dispatch<WpPresentation, GlobalData> for App {
    fn event(
        state: &mut Self,
        proxy: &WpPresentation,
        event: wp_presentation::Event,
        data: &GlobalData,
        conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.presentation_clock = Some(clk_id);
        }
        <PresentationTimeState as Dispatch<WpPresentation, GlobalData, App>>::event(
            state, proxy, event, data, conn, qh,
        );
    }
}

/// Decode a grant event.
pub(super) fn wire_grant(event: output_timeline::Event) -> Result<Grant, PresenterError> {
    let output_timeline::Event::Grant {
        epoch_hi,
        epoch_lo,
        sequence_hi,
        sequence_lo,
        latch_sec_hi,
        latch_sec_lo,
        latch_nsec,
        target_sec_hi,
        target_sec_lo,
        target_nsec,
        period_ns_hi,
        period_ns_lo,
    } = event
    else {
        return Err(PresenterError::Configuration("expected a frame grant"));
    };
    Ok(Grant {
        identity: Identity {
            epoch: join(epoch_hi, epoch_lo),
            sequence: join(sequence_hi, sequence_lo),
        },
        latch_deadline_ns: presentation_timestamp_nanos(
            join(latch_sec_hi, latch_sec_lo),
            latch_nsec,
        )?,
        presentation_target_ns: presentation_timestamp_nanos(
            join(target_sec_hi, target_sec_lo),
            target_nsec,
        )?,
        refresh_period_ns: join(period_ns_hi, period_ns_lo),
    })
}

fn join(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

wayland_client::delegate_noop!(App: ignore Manager);
wayland_client::delegate_noop!(App: ignore DenialSurfaceFrameTimelineV1);

// Removing an exact-mode global pauses its producers; it never selects portable
// mode mid-stream. Output removal similarly waits for a new subscription/epoch.
pub(super) struct Registry;
impl RegistryHandler<App> for Registry {
    fn remove_global(
        state: &mut App,
        _: &Connection,
        _: &QueueHandle<App>,
        _: u32,
        interface: &str,
    ) {
        if interface != Manager::interface().name {
            return;
        }
        if let Some(manager) = state.frame_timeline_manager.take() {
            manager.destroy();
            for object in state.tasks.keys().copied().collect::<Vec<_>>() {
                if state.tasks[&object].timeline.exact {
                    if let Err(error) = state.abort_timeline_work(object, Terminal::Epoch) {
                        state.fail(&error);
                    }
                    state.tasks.get_mut(&object).unwrap().timeline.stop();
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "frame_timeline_test.rs"]
mod tests;
