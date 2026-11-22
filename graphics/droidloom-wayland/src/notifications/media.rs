//! Android media sessions as MPRIS players. SystemUI turns a media
//! notification into its media controls instead of a notification row; this
//! does the same for the desktop: each one becomes an
//! `org.mpris.MediaPlayer2` player, with the app's own buttons on
//! `org.droidloom.MediaPlayer2.Actions`.
use super::{Media, Note};
use std::{
    collections::HashMap,
    fs, io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::Instant,
};
use zbus::{
    Connection, interface,
    object_server::SignalEmitter,
    zvariant::{ObjectPath, OwnedValue, Value},
};

const PATH: &str = "/org/mpris/MediaPlayer2";
/// A command for Android's notification listener, sent as one frame.
pub(super) type Commands = async_channel::Sender<serde_json::Value>;

/// What one player shows, and when its position was last reported.
struct Track {
    app: String,
    desktop_entry: String,
    /// The notification's small icon (white on clear, 48 px RGBA hex): the
    /// player shows it tinted, as Android's does.
    small_icon: String,
    media: Media,
    reported: Instant,
    /// Changes with the song, for `mpris:trackid` and `SetPosition`.
    id: u64,
    art_url: String,
}
impl Track {
    /// The position now, in microseconds, from the last report and the rate.
    fn position(&self) -> i64 {
        let mut ms = self.media.position;
        if self.media.playing {
            let elapsed = i64::try_from(self.reported.elapsed().as_millis()).unwrap_or(i64::MAX);
            ms = ms.saturating_add(elapsed.saturating_mul(i64::from(self.media.rate)) / 1000);
        }
        if self.media.duration > 0 {
            ms = ms.min(self.media.duration);
        }
        ms.max(0).saturating_mul(1000)
    }
    fn track_path(&self) -> String {
        format!("/org/droidloom/MediaPlayer2/Track/{}", self.id)
    }
}
type Shared = Arc<Mutex<Track>>;
fn lock(track: &Shared) -> MutexGuard<'_, Track> {
    track
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct Root {
    track: Shared,
}
#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    /// Denial opens the app through its desktop entry instead.
    fn raise(&self) {}
    fn quit(&self) {}
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn identity(&self) -> String {
        lock(&self.track).app.clone()
    }
    #[zbus(property)]
    fn desktop_entry(&self) -> String {
        lock(&self.track).desktop_entry.clone()
    }
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        vec![]
    }
    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        vec![]
    }
}

struct Player {
    key: String,
    track: Shared,
    commands: Commands,
}
impl Player {
    async fn send(&self, command: &str, extra: Option<(&str, serde_json::Value)>) {
        let mut frame = serde_json::json!({"type":"media","key":self.key,"command":command});
        if let Some((name, value)) = extra {
            frame[name] = value;
        }
        let _ = self.commands.try_send(frame);
    }
}
#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    async fn next(&self) {
        self.send("next", None).await;
    }
    async fn previous(&self) {
        self.send("previous", None).await;
    }
    async fn pause(&self) {
        self.send("pause", None).await;
    }
    async fn play(&self) {
        self.send("play", None).await;
    }
    async fn play_pause(&self) {
        let playing = lock(&self.track).media.playing;
        self.send(if playing { "pause" } else { "play" }, None)
            .await;
    }
    async fn stop(&self) {
        self.send("pause", None).await;
    }
    async fn seek(&self, offset: i64) {
        let target = {
            let track = lock(&self.track);
            if !track.media.seek {
                return;
            }
            track
                .position()
                .saturating_add(offset)
                .clamp(0, track.media.duration * 1000)
        };
        self.send("seek", Some(("position", (target / 1000).into())))
            .await;
    }
    async fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {
        {
            let track = lock(&self.track);
            if !track.media.seek
                || track_id.as_str() != track.track_path()
                || !(0..=track.media.duration * 1000).contains(&position)
            {
                return;
            }
        }
        self.send("seek", Some(("position", (position / 1000).into())))
            .await;
    }
    fn open_uri(&self, _uri: &str) {}
    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;
    #[zbus(property)]
    fn playback_status(&self) -> &'static str {
        if lock(&self.track).media.playing {
            "Playing"
        } else {
            "Paused"
        }
    }
    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn volume(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let track = lock(&self.track);
        let mut metadata = HashMap::new();
        let mut put = |key: &str, value: Value<'_>| {
            if let Ok(value) = value.try_to_owned() {
                metadata.insert(key.to_owned(), value);
            }
        };
        if let Ok(path) = ObjectPath::try_from(track.track_path()) {
            put("mpris:trackid", Value::from(path));
        }
        if track.media.duration > 0 {
            put("mpris:length", Value::from(track.media.duration * 1000));
        }
        put("xesam:title", Value::from(track.media.title.as_str()));
        if !track.media.artist.is_empty() {
            put(
                "xesam:artist",
                Value::from(vec![track.media.artist.as_str()]),
            );
        }
        if !track.media.album.is_empty() {
            put("xesam:album", Value::from(track.media.album.as_str()));
        }
        if !track.art_url.is_empty() {
            put("mpris:artUrl", Value::from(track.art_url.as_str()));
        }
        metadata
    }
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        lock(&self.track).position()
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        lock(&self.track).media.next
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        lock(&self.track).media.previous
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        lock(&self.track).media.play
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        lock(&self.track).media.pause
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        lock(&self.track).media.seek
    }
    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}

/// The app's own buttons (`PlaybackState.CustomAction`): a like button,
/// a device picker. Not part of MPRIS.
struct Actions {
    key: String,
    track: Shared,
    commands: Commands,
}
#[interface(name = "org.droidloom.MediaPlayer2.Actions")]
impl Actions {
    fn invoke(&self, id: &str) {
        if lock(&self.track).media.custom.iter().any(|a| a.id == id) {
            let _ = self.commands.try_send(
                serde_json::json!({"type":"media","key":self.key,"command":"custom","action":id}),
            );
        }
    }
    /// The app's small icon, 48 px RGBA (white on clear), or empty.
    #[zbus(property)]
    fn icon(&self) -> Vec<u8> {
        hex::decode(&lock(&self.track).small_icon).unwrap_or_default()
    }
    /// Each button: its id, its label, and its square icon as RGBA (or empty),
    /// 2 px per dp at the app's own size (48 px, 24 dp, from older bundles).
    #[zbus(property)]
    fn actions(&self) -> Vec<(String, String, Vec<u8>)> {
        lock(&self.track)
            .media
            .custom
            .iter()
            .map(|a| {
                (
                    a.id.clone(),
                    a.label.clone(),
                    hex::decode(&a.icon).unwrap_or_default(),
                )
            })
            .collect()
    }
}

struct Live {
    conn: Connection,
    name: String,
    track: Shared,
    art: Option<PathBuf>,
}

/// The players, one bus name each (MPRIS has one player per name).
pub(super) struct Players {
    commands: Commands,
    live: HashMap<String, Live>,
    next: u64,
    tracks: u64,
    /// Each artwork file gets a new name, so viewers never show a cached one.
    arts: u64,
}
impl Players {
    pub(super) fn new(commands: Commands) -> Self {
        Self {
            commands,
            live: HashMap::new(),
            next: 0,
            tracks: 0,
            arts: 0,
        }
    }

    /// Shows [note]'s player, or changes it.
    pub(super) async fn update(&mut self, note: &Note, desktop_entry: Option<String>) {
        let Some(media) = note.media.clone() else {
            return;
        };
        if let Err(error) = self
            .apply(note, media, desktop_entry.unwrap_or_default())
            .await
        {
            eprintln!("Droidloom media: player unavailable ({error})");
        }
    }

    async fn apply(
        &mut self,
        note: &Note,
        media: Media,
        desktop_entry: String,
    ) -> zbus::Result<()> {
        let Some(live) = self.live.get_mut(&note.key) else {
            self.next += 1;
            self.tracks += 1;
            self.arts += 1;
            let (art, art_url) = write_art(self.arts, &media.art);
            let track = Arc::new(Mutex::new(Track {
                app: note.app.clone(),
                desktop_entry,
                small_icon: player_icon(note, &media),
                media,
                reported: Instant::now(),
                id: self.tracks,
                art_url,
            }));
            let package: String = note
                .package
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            let name = format!("org.mpris.MediaPlayer2.droidloom_{package}.i{}", self.next);
            let result = zbus::connection::Builder::session()?
                .name(name.as_str())?
                .serve_at(
                    PATH,
                    Root {
                        track: track.clone(),
                    },
                )?
                .serve_at(
                    PATH,
                    Player {
                        key: note.key.clone(),
                        track: track.clone(),
                        commands: self.commands.clone(),
                    },
                )?
                .serve_at(
                    PATH,
                    Actions {
                        key: note.key.clone(),
                        track: track.clone(),
                        commands: self.commands.clone(),
                    },
                )?
                .build()
                .await;
            let conn = match result {
                Ok(conn) => conn,
                Err(error) => {
                    if let Some(path) = art {
                        let _ = fs::remove_file(path);
                    }
                    return Err(error);
                }
            };
            self.live.insert(
                note.key.clone(),
                Live {
                    conn,
                    name,
                    track,
                    art,
                },
            );
            return Ok(());
        };
        // What changed, decided under the lock; the signals go out after it.
        let (metadata, status, abilities, actions, icon, seeked) = {
            let mut track = lock(&live.track);
            let old = &track.media;
            let song =
                old.title != media.title || old.artist != media.artist || old.album != media.album;
            let art_changed = old.art != media.art;
            let metadata = song || art_changed || old.duration != media.duration;
            let status = old.playing != media.playing;
            let abilities = (old.play, old.pause, old.next, old.previous, old.seek)
                != (
                    media.play,
                    media.pause,
                    media.next,
                    media.previous,
                    media.seek,
                );
            let actions = old.custom != media.custom;
            let icon = track.small_icon != player_icon(note, &media);
            // A jump away from where the position was heading: a seek.
            let expected = track.position() / 1000;
            let seeked = song || (expected - media.position).abs() > 1500;
            if song {
                self.tracks += 1;
                track.id = self.tracks;
            }
            if art_changed {
                if let Some(path) = live.art.take() {
                    let _ = fs::remove_file(path);
                }
                self.arts += 1;
                let (art, url) = write_art(self.arts, &media.art);
                live.art = art;
                track.art_url = url;
            }
            track.app.clone_from(&note.app);
            track.small_icon = player_icon(note, &media);
            track.media = media;
            track.reported = Instant::now();
            (
                metadata,
                status,
                abilities,
                actions,
                icon,
                seeked.then(|| track.position()),
            )
        };
        let server = live.conn.object_server();
        let player = server.interface::<_, Player>(PATH).await?;
        let emitter = player.signal_emitter();
        let iface = player.get().await;
        if metadata {
            iface.metadata_changed(emitter).await?;
        }
        if status {
            iface.playback_status_changed(emitter).await?;
        }
        if abilities {
            iface.can_play_changed(emitter).await?;
            iface.can_pause_changed(emitter).await?;
            iface.can_go_next_changed(emitter).await?;
            iface.can_go_previous_changed(emitter).await?;
            iface.can_seek_changed(emitter).await?;
        }
        if let Some(position) = seeked {
            Player::seeked(emitter, position).await?;
        }
        drop(iface);
        if actions || icon {
            let buttons = server.interface::<_, Actions>(PATH).await?;
            let iface = buttons.get().await;
            if actions {
                iface.actions_changed(buttons.signal_emitter()).await?;
            }
            if icon {
                iface.icon_changed(buttons.signal_emitter()).await?;
            }
        }
        Ok(())
    }

    /// The session ended or its notification went away.
    pub(super) async fn remove(&mut self, key: &str) {
        if let Some(live) = self.live.remove(key) {
            let _ = live.conn.release_name(live.name.as_str()).await;
            if let Some(path) = live.art {
                let _ = fs::remove_file(path);
            }
        }
    }

    pub(super) fn contains(&self, key: &str) -> bool {
        self.live.contains_key(key)
    }
}
impl Drop for Players {
    fn drop(&mut self) {
        for live in self.live.values() {
            if let Some(path) = &live.art {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// The icon the player shows: the 96 px one the player carries, else the
/// notification's 48 px small icon.
fn player_icon(note: &Note, media: &Media) -> String {
    if media.icon.is_empty() {
        note.small_icon.clone()
    } else {
        media.icon.clone()
    }
}

/// The artwork (JPEG as hex) in the user's runtime directory, for
/// `mpris:artUrl`: its path and its `file://` URL, or none.
fn write_art(id: u64, hex_jpeg: &str) -> (Option<PathBuf>, String) {
    let Ok(bytes) = hex::decode(hex_jpeg) else {
        return (None, String::new());
    };
    if bytes.is_empty() {
        return (None, String::new());
    }
    let write = || -> io::Result<PathBuf> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or(io::ErrorKind::NotFound)?;
        let dir = PathBuf::from(runtime).join("droidloom-media");
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e),
        }
        let path = dir.join(format!("art-{id:x}.jpg"));
        let tmp = path.with_extension("new");
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        fs::rename(tmp, &path)?;
        Ok(path)
    };
    match write() {
        Ok(path) => {
            let url = url::Url::from_file_path(&path)
                .map(String::from)
                .unwrap_or_default();
            (Some(path), url)
        }
        Err(_) => (None, String::new()),
    }
}
