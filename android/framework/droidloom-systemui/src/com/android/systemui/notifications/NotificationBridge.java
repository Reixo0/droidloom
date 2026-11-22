/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.notifications;

import android.app.ActivityOptions;
import android.app.INotificationManager;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.drawable.Drawable;
import android.media.MediaMetadata;
import android.media.session.MediaController;
import android.media.session.MediaSession;
import android.media.session.PlaybackState;
import com.android.droidloom.runtime.HostChannel;
import android.os.Bundle;
import android.os.Handler;
import android.os.ServiceManager;
import android.os.SystemClock;
import android.os.UserHandle;
import android.provider.Settings;
import android.service.notification.NotificationListenerService;
import android.service.notification.StatusBarNotification;
import android.util.Log;
import android.util.LruCache;
import java.io.DataInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.PrintWriter;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.UUID;
import org.json.JSONArray;
import org.json.JSONObject;

/** Event-driven listener; PendingIntents stay inside Android and never cross the socket. */
public final class NotificationBridge extends NotificationListenerService implements AutoCloseable {
    private static final String TAG = "DroidloomNotifications";
    private static final int MAX_ACTIVE = 256;
    // Room for a 96 px large icon in hex next to the longest texts.
    private static final int MAX_FRAME = 1<<20;
    private final Context context;
    private final Handler main;
    private final Object lock = new Object();
    private final LinkedHashMap<String, Entry> active = new LinkedHashMap<>();
    // Keys whose latest post should alert and has not reached the host yet.
    private final HashSet<String> alerting = new HashSet<>();
    private final LruCache<String, String> icons = new LruCache<>(32);
    private final LruCache<String, String> components = new LruCache<>(32);
    private final String session = UUID.randomUUID().toString();
    /** The media session behind each media notification, as SystemUI's MediaDataManager follows it. */
    private final HashMap<String, Media> media = new HashMap<>();
    private long generation;
    private boolean dirty;
    private volatile boolean stopped, connected, listening;
    private volatile HostChannel socket;
    private static final class Entry {
        final StatusBarNotification sbn;
        final String version;
        final int importance;
        /** The notification's channel as ranked, or null. */
        final NotificationChannel channel;
        Entry(StatusBarNotification sbn, String version, int importance, NotificationChannel channel) {
            this.sbn = sbn; this.version = version; this.importance = importance; this.channel = channel;
        }
    }
    /**
     * A media notification's session: every playback or metadata change reposts the
     * notification, so the host's player follows it.
     */
    private final class Media extends MediaController.Callback {
        final String key;
        final MediaController controller;
        /** The last artwork sent and its JPEG as hex, so an unchanged picture is not encoded again. */
        Bitmap art; String artHex="";
        Media(String key, MediaSession.Token token) {
            this.key=key; controller=new MediaController(context,token);
            controller.registerCallback(this,main);
        }
        @Override public void onPlaybackStateChanged(PlaybackState state) { touch(key); }
        @Override public void onMetadataChanged(MediaMetadata metadata) { touch(key); }
        @Override public void onSessionDestroyed() { touch(key); }
        void close() { try { controller.unregisterCallback(this); } catch(Exception ignored) { } }
    }
    /** A new version of [key] with the same notification (its session changed). */
    private void touch(String key) {
        synchronized(lock) {
            Entry entry=active.get(key); if(entry==null) return;
            active.put(key,new Entry(entry.sbn,session+"-"+(++generation),entry.importance,entry.channel));
            dirty=true; lock.notifyAll();
        }
    }
    private void forget(String key) { Media old=media.remove(key); if(old!=null) old.close(); }
    private void forgetAll() { for(Media m:media.values()) m.close(); media.clear(); }
    /** Follows the session a media notification names (MediaDataManager.onNotificationAdded). */
    private void follow(StatusBarNotification sbn) {
        MediaSession.Token token=sbn.getNotification().extras.getParcelable(Notification.EXTRA_MEDIA_SESSION,MediaSession.Token.class);
        Media current=media.get(sbn.getKey());
        if(token==null) { forget(sbn.getKey()); return; }
        if(current!=null && current.controller.getSessionToken().equals(token)) return;
        forget(sbn.getKey());
        try { media.put(sbn.getKey(),new Media(sbn.getKey(),token)); }
        catch(Exception e) { Log.w(TAG,"Media session unavailable: "+e.getClass().getSimpleName()); }
    }
    public NotificationBridge(Context context) { this.context=context; main=new Handler(context.getMainLooper()); }
    public void start() {
        try {
            registerAsSystemService(context, new ComponentName(context, NotificationBridge.class), UserHandle.USER_SYSTEM);
            Log.i(TAG, "DROIDLOOM_NOTIFICATIONS_ABI=4;");
            Thread worker = new Thread(this::connect, "droidloom-notifications");
            worker.setDaemon(true); worker.start();
        } catch (Exception e) { Log.e(TAG, "Notification listener registration failed: " + e.getClass().getSimpleName()); }
    }
    @Override public void onListenerConnected() {
        synchronized (lock) {
            active.clear(); alerting.clear(); forgetAll(); listening=true;
            StatusBarNotification[] all=getActiveNotifications();
            if (all!=null) for (StatusBarNotification sbn:all) put(sbn, getCurrentRanking(), false);
            dirty=true; lock.notifyAll();
        }
    }
    @Override public void onListenerDisconnected() {
        listening=false;
        disconnect();
    }
    @Override public void onNotificationPosted(StatusBarNotification sbn, RankingMap ranking) {
        boolean fresh;
        synchronized(lock) { fresh=!active.containsKey(sbn.getKey()); put(sbn,ranking,true); fresh&=active.containsKey(sbn.getKey()); dirty=true; lock.notifyAll(); }
        if(fresh) fullScreen(sbn,ranking);
    }
    /**
     * SystemUI's full-screen intent: a ringing alarm or call opens its screen.
     * Android cannot see whether the host is locked or in use, so it always
     * opens; the host shows that window over its lock screen and wakes it.
     */
    private void fullScreen(StatusBarNotification sbn, RankingMap map) {
        Notification n=sbn.getNotification();
        if(n.fullScreenIntent==null) return;
        Ranking rank=new Ranking();
        if(map!=null && map.getRanking(sbn.getKey(),rank) && rank.getImportance()<NotificationManager.IMPORTANCE_HIGH) return;
        try {
            ActivityOptions options=ActivityOptions.makeBasic().setPendingIntentBackgroundActivityStartMode(ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOWED);
            n.fullScreenIntent.send(context,0,null,null,null,null,options.toBundle());
        } catch(Exception e) { Log.w(TAG,"Full-screen intent unavailable: "+e.getClass().getSimpleName()); }
    }
    /** A channel's importance changed (from the notification settings, say): repost quietly. */
    @Override public void onNotificationRankingUpdate(RankingMap map) {
        synchronized(lock) {
            for(Entry entry:new ArrayList<>(active.values())) {
                Ranking rank=new Ranking();
                if(map!=null && map.getRanking(entry.sbn.getKey(),rank) && rank.getImportance()!=entry.importance) {
                    put(entry.sbn,map,false); dirty=true;
                }
            }
            lock.notifyAll();
        }
    }
    @Override public void onNotificationRemoved(StatusBarNotification sbn) {
        synchronized(lock) { active.remove(sbn.getKey()); alerting.remove(sbn.getKey()); forget(sbn.getKey()); dirty=true; lock.notifyAll(); }
    }
    private void put(StatusBarNotification sbn, RankingMap map, boolean live) {
        Ranking rank = new Ranking();
        boolean ranked = map!=null && map.getRanking(sbn.getKey(),rank);
        int importance=ranked?rank.getImportance():NotificationManager.IMPORTANCE_DEFAULT;
        if (ranked && (rank.isSuspended() || importance==NotificationManager.IMPORTANCE_NONE)) {
            active.remove(sbn.getKey()); alerting.remove(sbn.getKey()); forget(sbn.getKey()); return;
        }
        boolean update=active.containsKey(sbn.getKey());
        if (!update && active.size()>=MAX_ACTIVE) { String oldest=active.keySet().iterator().next(); active.remove(oldest); alerting.remove(oldest); forget(oldest); }
        active.put(sbn.getKey(),new Entry(sbn,session+"-"+(++generation),importance,ranked?rank.getChannel():null));
        follow(sbn);
        if (live && alerts(sbn,rank,ranked,importance,update)) alerting.add(sbn.getKey());
    }
    /** SystemUI's heads-up decision: HIGH importance, and the post says something new. */
    private static boolean alerts(StatusBarNotification sbn, Ranking rank, boolean ranked, int importance, boolean update) {
        Notification n=sbn.getNotification();
        if (importance<NotificationManager.IMPORTANCE_HIGH || n.isGroupSummary() || n.suppressAlertingDueToGrouping()) return false;
        if (update && (n.flags & Notification.FLAG_ONLY_ALERT_ONCE)!=0) return false;
        return !ranked || ((rank.getSuppressedVisualEffects() & NotificationManager.Policy.SUPPRESSED_EFFECT_PEEK)==0 && rank.isTextChanged());
    }
    private void connect() {
        com.android.droidloom.runtime.CpuPlacement.background();
        while (!stopped) {
            try (HostChannel current = HostChannel.open(HostChannel.NOTIFICATIONS)) {
                socket=current;
                DataOutputStream out=new DataOutputStream(current.getOutputStream());
                send(out,new JSONObject().put("type","hello").put("abi",4));
                current.setSoTimeout(5000);
                DataInputStream input=new DataInputStream(current.getInputStream());
                int helloLength=input.readInt();
                if(helloLength<=0 || helloLength>4096) throw new IllegalArgumentException();
                byte[] helloBytes=new byte[helloLength]; input.readFully(helloBytes);
                JSONObject hello=new JSONObject(new String(helloBytes,StandardCharsets.UTF_8));
                if(!"hello".equals(hello.optString("type")) || hello.optInt("abi")!=4) throw new IllegalArgumentException();
                current.setSoTimeout(0);
                connected=true;
                Thread writer=new Thread(() -> writeSnapshots(current,out),"droidloom-notifications-out");
                writer.setDaemon(true); writer.start();
                synchronized(lock) { dirty=true; lock.notifyAll(); }
                try {
                    while(!stopped) {
                        int length=input.readInt();
                        if(length<=0 || length>4096) throw new IllegalArgumentException();
                        byte[] bytes=new byte[length]; input.readFully(bytes);
                        JSONObject message=new JSONObject(new String(bytes,StandardCharsets.UTF_8));
                        main.post(() -> command(message));
                    }
                } finally {
                    connected=false; closeSocket(current);
                    synchronized(lock) { lock.notifyAll(); }
                    writer.join();
                }
            } catch(Exception e) {
                if(!stopped) Log.w(TAG,"Notification transport disconnected: "+e.getClass().getSimpleName());
            } finally { connected=false; socket=null; }
            if(!stopped) try { Thread.sleep(2000); } catch(InterruptedException ignored) { }
        }
    }
    private void writeSnapshots(HostChannel current, DataOutputStream out) {
        com.android.droidloom.runtime.CpuPlacement.background();
        LinkedHashMap<String,String> sent = new LinkedHashMap<>();
        boolean initial = true;
        try {
            while(!stopped && connected) {
                ArrayList<Entry> entries;
                HashSet<String> alerts;
                synchronized(lock) {
                    while(!stopped && connected && (!dirty || !listening)) lock.wait();
                    if(stopped || !connected) return;
                    entries=new ArrayList<>(active.values()); dirty=false;
                    alerts=new HashSet<>(alerting); alerting.clear();
                }
                LinkedHashMap<String,String> next = new LinkedHashMap<>();
                if(initial) send(out,new JSONObject().put("type","begin"));
                else {
                    // Retire entries before adding replacements so a full host set
                    // never transiently exceeds MAX_ACTIVE during a delta update.
                    HashSet<String> present = new HashSet<>();
                    for(Entry entry:entries) present.add(entry.sbn.getKey());
                    for(String key:sent.keySet()) if(!present.contains(key))
                        send(out,new JSONObject().put("type","remove").put("key",key).put("version",sent.get(key)));
                }
                for(Entry entry:entries) {
                    String key=entry.sbn.getKey();
                    if(!initial && entry.version.equals(sent.get(key))) {
                        next.put(key,entry.version); continue;
                    }
                    try {
                        send(out,new JSONObject().put("type","post").put("notification",encode(entry,alerts.contains(key))));
                        next.put(key,entry.version);
                    } catch(org.json.JSONException | android.content.pm.PackageManager.NameNotFoundException e) {
                        // Preserve the acknowledged version so a later event retries this entry.
                        // An app removed during encoding is retired by the following removal event.
                        if(sent.containsKey(key)) next.put(key,sent.get(key));
                    }
                }
                if(initial) send(out,new JSONObject().put("type","end"));
                sent=next; initial=false;
            }
        } catch(Exception e) { closeSocket(current); }
    }
    private JSONObject encode(Entry entry, boolean alert) throws Exception {
        StatusBarNotification sbn=entry.sbn; Notification n=sbn.getNotification(); Bundle extras=n.extras;
        String title=text(extras.getCharSequence(Notification.EXTRA_TITLE),2048);
        CharSequence big=extras.getCharSequence(Notification.EXTRA_BIG_TEXT);
        String body=text(big!=null?big:extras.getCharSequence(Notification.EXTRA_TEXT),8192);
        if(body.isEmpty()) {
            CharSequence[] lines=extras.getCharSequenceArray(Notification.EXTRA_TEXT_LINES);
            if(lines!=null) body=text(android.text.TextUtils.join("\n",lines),8192);
        }
        String component=component(sbn);
        JSONArray actions=new JSONArray();
        // Without a content intent, a tap still opens the app at its launcher activity.
        if(n.contentIntent!=null || !component.isEmpty()) actions.put(new JSONObject().put("key","default").put("label","Open"));
        if(n.actions!=null) for(int i=0;i<Math.min(n.actions.length,16);i++) {
            Notification.Action action=n.actions[i];
            // The standard D-Bus interface has no portable inline-reply field.
            if(action.actionIntent!=null && action.getRemoteInputs()==null)
                actions.put(new JSONObject().put("key","action-"+i).put("label",text(action.title,256)));
        }
        int max=extras.getInt(Notification.EXTRA_PROGRESS_MAX,0), value=extras.getInt(Notification.EXTRA_PROGRESS,0);
        int progress=max>0 && !extras.getBoolean(Notification.EXTRA_PROGRESS_INDETERMINATE)
                ? (int)Math.max(0,Math.min(100,(long)value*100/max)) : -1;
        android.content.pm.ApplicationInfo info=context.getPackageManager().getApplicationInfo(sbn.getPackageName(),0);
        String label=context.getPackageManager().getApplicationLabel(info).toString();
        NotificationChannel channel=entry.channel;
        Media session; synchronized(lock) { session=media.get(sbn.getKey()); }
        JSONObject player=session==null?null:player(session,n,extras);
        return new JSONObject().put("key",sbn.getKey()).put("version",entry.version).put("package",sbn.getPackageName())
                .put("app",text(label,256)).put("title",title).put("body",body).put("actions",actions)
                // Android HIGH requests a heads-up alert; it does not mean a critical
                // desktop event that must remain visible until explicitly dismissed.
                .put("urgency",entry.importance<=NotificationManager.IMPORTANCE_LOW?0:1)
                .put("ongoing",sbn.isOngoing()).put("clearable",sbn.isClearable())
                .put("alert",alert)
                // Ungrouped notifications get a unique group key; the host groups those by app.
                .put("group",sbn.isGroup()?text(sbn.getGroupKey(),512):"")
                .put("summary",n.isGroupSummary())
                .put("when",n.when!=0?n.when:sbn.getPostTime())
                .put("progress",progress)
                .put("category",n.category==null?"":text(n.category,64))
                // A notification for every user (-1) shows as the current one's.
                .put("user",sbn.getNormalizedUserId()).put("component",component)
                // The host names the app icon through its desktop entry when it has one.
                .put("icon",component.isEmpty()?icon(sbn.getPackageName()):"")
                // A player's artwork travels with the player.
                .put("large_icon",player!=null?"":largeIcon(n))
                // The status bar icon, white on clear; the host tints it.
                .put("small_icon",smallIcon(n))
                // The notification settings: the channel's name, and whether the user may
                // change its importance (SystemUI's NotificationInfo rules).
                .put("channel",channel==null?"":text(channel.getName(),256))
                .put("configurable",configurable(channel,info))
                .put("messages",messages(extras))
                .put("media",player==null?JSONObject.NULL:player);
    }
    /**
     * The player SystemUI's media controls show for a session (MediaDataManager,
     * MediaActions): what plays, where it is, and the buttons the app offers.
     */
    private JSONObject player(Media media, Notification n, Bundle extras) throws Exception {
        MediaController controller=media.controller;
        PlaybackState state=controller.getPlaybackState();
        MediaMetadata meta=controller.getMetadata();
        int playback=state==null?PlaybackState.STATE_NONE:state.getState();
        boolean playing=playback==PlaybackState.STATE_PLAYING || playback==PlaybackState.STATE_FAST_FORWARDING
                || playback==PlaybackState.STATE_REWINDING;
        boolean buffering=playback==PlaybackState.STATE_BUFFERING || playback==PlaybackState.STATE_CONNECTING;
        long duration=meta==null?0:Math.max(0,meta.getLong(MediaMetadata.METADATA_KEY_DURATION));
        long position=state==null?0:Math.max(0,state.getPosition());
        float speed=state==null?0:state.getPlaybackSpeed();
        if(playing && state.getLastPositionUpdateTime()>0)
            position+=(long)((SystemClock.elapsedRealtime()-state.getLastPositionUpdateTime())*speed);
        if(duration>0) position=Math.min(position,duration);
        long actions=state==null?0:state.getActions();
        // As MediaDataManager: the session's metadata, else the notification's text.
        String title=first(meta,MediaMetadata.METADATA_KEY_DISPLAY_TITLE,MediaMetadata.METADATA_KEY_TITLE);
        if(title.isEmpty()) title=text(extras.getCharSequence(Notification.EXTRA_TITLE),1024);
        String artist=first(meta,MediaMetadata.METADATA_KEY_DISPLAY_SUBTITLE,MediaMetadata.METADATA_KEY_ARTIST,MediaMetadata.METADATA_KEY_ALBUM_ARTIST);
        if(artist.isEmpty()) artist=text(extras.getCharSequence(Notification.EXTRA_TEXT),1024);
        JSONArray custom=new JSONArray();
        if(state!=null) for(PlaybackState.CustomAction action:state.getCustomActions()) {
            if(custom.length()>=4) break;
            custom.put(new JSONObject().put("id",text(action.getAction(),256)).put("label",text(action.getName(),256))
                    .put("icon",actionIcon(controller.getPackageName(),action.getIcon())));
        }
        return new JSONObject().put("playing",playing).put("buffering",buffering)
                .put("position",position).put("rate",Math.round((playing?speed:0)*1000)).put("duration",duration)
                .put("title",title).put("artist",artist).put("album",first(meta,MediaMetadata.METADATA_KEY_ALBUM))
                .put("art",art(media,meta,n))
                // The player draws the small icon at 24 dp: 96 px keeps it sharp.
                .put("icon",smallIcon(n,96))
                .put("play",(actions&(PlaybackState.ACTION_PLAY|PlaybackState.ACTION_PLAY_PAUSE))!=0)
                .put("pause",(actions&(PlaybackState.ACTION_PAUSE|PlaybackState.ACTION_PLAY_PAUSE))!=0)
                .put("next",(actions&PlaybackState.ACTION_SKIP_TO_NEXT)!=0)
                .put("previous",(actions&PlaybackState.ACTION_SKIP_TO_PREVIOUS)!=0)
                .put("seek",duration>0 && (actions&PlaybackState.ACTION_SEEK_TO)!=0)
                .put("custom",custom);
    }
    private static String first(MediaMetadata meta,String... keys) {
        if(meta==null) return "";
        for(String key:keys) { String value=text(meta.getText(key),1024); if(!value.isEmpty()) return value; }
        return "";
    }
    /** The album art as a JPEG of at most 384 px, as hex; the session's, else the notification's large icon. */
    private String art(Media media, MediaMetadata meta, Notification n) {
        Bitmap bitmap=null;
        if(meta!=null) bitmap=meta.getBitmap(MediaMetadata.METADATA_KEY_ART);
        if(bitmap==null && meta!=null) bitmap=meta.getBitmap(MediaMetadata.METADATA_KEY_ALBUM_ART);
        if(bitmap==null && meta!=null) bitmap=meta.getBitmap(MediaMetadata.METADATA_KEY_DISPLAY_ICON);
        try {
            if(bitmap==null && n.getLargeIcon()!=null) {
                Drawable drawable=n.getLargeIcon().loadDrawable(context);
                if(drawable instanceof android.graphics.drawable.BitmapDrawable b) bitmap=b.getBitmap();
            }
            if(bitmap==null) { media.art=null; media.artHex=""; return ""; }
            if(media.art!=null && (media.art==bitmap || media.art.sameAs(bitmap))) return media.artHex;
            float scale=Math.min(1f,384f/Math.max(bitmap.getWidth(),bitmap.getHeight()));
            Bitmap small=Bitmap.createScaledBitmap(bitmap,Math.max(1,Math.round(bitmap.getWidth()*scale)),
                    Math.max(1,Math.round(bitmap.getHeight()*scale)),true);
            String hex="";
            for(int quality:new int[]{85,60,40}) {
                ByteArrayOutputStream out=new ByteArrayOutputStream();
                small.compress(Bitmap.CompressFormat.JPEG,quality,out);
                if(out.size()<=48000) { hex=hexBytes(out.toByteArray()); break; }
            }
            if(small!=bitmap) small.recycle();
            media.art=bitmap; media.artHex=hex; return hex;
        } catch(Exception e) { return ""; }
    }
    /**
     * A custom action's icon as the media player's button shows it: at the
     * drawable's own size (centerInside never enlarges it), as a square of
     * its larger side at 2 px per dp (at most 48 dp, the button), RGBA as
     * hex. Without an intrinsic size it is 24 dp (48 px).
     */
    private String actionIcon(String pkg,int id) {
        try {
            android.content.res.Resources res=context.getPackageManager().getResourcesForApplication(pkg);
            Drawable drawable=res.getDrawable(id,null);
            if(drawable==null) return "";
            float toPixels=2f/res.getDisplayMetrics().density;
            int width=drawable.getIntrinsicWidth(), height=drawable.getIntrinsicHeight();
            int w=width>0?Math.min(96,Math.max(1,Math.round(width*toPixels))):48;
            int h=height>0?Math.min(96,Math.max(1,Math.round(height*toPixels))):48;
            final int size=Math.max(w,h);
            int left=(size-w)/2, top=(size-h)/2;
            Bitmap bitmap=Bitmap.createBitmap(size,size,Bitmap.Config.ARGB_8888);
            drawable.setBounds(left,top,left+w,top+h); drawable.draw(new Canvas(bitmap));
            int[] pixels=new int[size*size]; bitmap.getPixels(pixels,0,size,0,0,size,size); bitmap.recycle();
            StringBuilder out=new StringBuilder(size*size*8);
            for(int pixel:pixels) { hex(out,(pixel>>16)&255); hex(out,(pixel>>8)&255); hex(out,pixel&255); hex(out,(pixel>>>24)&255); }
            return out.toString();
        } catch(Exception e) { return ""; }
    }
    private static String hexBytes(byte[] bytes) {
        StringBuilder out=new StringBuilder(bytes.length*2);
        for(byte b:bytes) hex(out,b&255);
        return out.toString();
    }
    /** A media button pressed on the host's player (MediaControlPanel's actions). */
    private void mediaCommand(JSONObject message) throws Exception {
        Media m; synchronized(lock) { m=media.get(message.getString("key")); }
        if(m==null) return;
        MediaController.TransportControls controls=m.controller.getTransportControls();
        switch(message.getString("command")) {
            case "play" -> controls.play();
            case "pause" -> controls.pause();
            case "next" -> controls.skipToNext();
            case "previous" -> controls.skipToPrevious();
            case "seek" -> controls.seekTo(Math.max(0,message.getLong("position")));
            case "custom" -> {
                PlaybackState state=m.controller.getPlaybackState();
                String id=message.getString("action");
                if(state!=null) for(PlaybackState.CustomAction action:state.getCustomActions())
                    if(action.getAction().equals(id)) { controls.sendCustomAction(action,action.getExtras()); break; }
            }
            default -> { }
        }
    }
    /** The launcher activity Droidloom's desktop entry for this package names, or "". */
    private String component(StatusBarNotification sbn) {
        String cached=components.get(sbn.getPackageName()); if(cached!=null) return cached;
        Intent launch=launchIntent(sbn);
        String component=launch==null || launch.getComponent()==null ? "" : launch.getComponent().flattenToString();
        components.put(sbn.getPackageName(),component); return component;
    }
    private String icon(String pkg) {
        String cached=icons.get(pkg); if(cached!=null) return cached;
        String pixels="";
        try { pixels=pixels(context.getPackageManager().getApplicationIcon(pkg),48); } catch(Exception ignored) { }
        icons.put(pkg,pixels); return pixels;
    }
    private String largeIcon(Notification n) {
        if(n.getLargeIcon()==null) return "";
        try { return pixels(n.getLargeIcon().loadDrawable(context),96); } catch(Exception e) { return ""; }
    }
    private static boolean configurable(NotificationChannel channel, android.content.pm.ApplicationInfo info) {
        return channel!=null && !channel.isImportanceLockedByCriticalDeviceFunction()
                && (channel.isBlockable() || !info.isSystemApp());
    }
    /** The notification settings Denial shows on a long press. */
    private void control(Entry entry, String action) throws Exception {
        StatusBarNotification sbn=entry.sbn; NotificationChannel channel=entry.channel;
        if(action.equals("x-denial-settings")) {
            Intent intent=channel!=null
                    ? new Intent(Settings.ACTION_CHANNEL_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_CHANNEL_ID,channel.getId())
                    : new Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS);
            intent.putExtra(Settings.EXTRA_APP_PACKAGE,sbn.getPackageName()).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
            context.startActivityAsUser(intent,sbn.getUser());
            return;
        }
        int importance=action.equals("x-denial-alerting")?NotificationManager.IMPORTANCE_DEFAULT
                : action.equals("x-denial-silent")?NotificationManager.IMPORTANCE_LOW
                : action.equals("x-denial-turn-off")?NotificationManager.IMPORTANCE_NONE : -1;
        android.content.pm.ApplicationInfo info=context.getPackageManager().getApplicationInfo(sbn.getPackageName(),0);
        if(importance<0 || !configurable(channel,info)) return;
        // As SystemUI's NotificationInfo applies its choice.
        NotificationChannel update=channel.copy();
        update.setImportance(importance);
        update.lockFields(NotificationChannel.USER_LOCKED_IMPORTANCE);
        INotificationManager.Stub.asInterface(ServiceManager.getService("notification"))
                .updateNotificationChannelForPackage(sbn.getPackageName(),sbn.getUid(),update);
    }
    private String smallIcon(Notification n) { return smallIcon(n,48); }
    private String smallIcon(Notification n,int size) {
        if(n.getSmallIcon()==null) return "";
        try { return pixels(n.getSmallIcon().loadDrawable(context),size); } catch(Exception e) { return ""; }
    }
    private static JSONArray messages(Bundle extras) throws org.json.JSONException {
        JSONArray messages=new JSONArray();
        android.os.Parcelable[] raw=extras.getParcelableArray(Notification.EXTRA_MESSAGES);
        if(raw==null) return messages;
        java.util.List<Notification.MessagingStyle.Message> all=Notification.MessagingStyle.Message.getMessagesFromBundleArray(raw);
        for(int i=Math.max(0,all.size()-5);i<all.size();i++) {
            Notification.MessagingStyle.Message message=all.get(i);
            CharSequence sender=message.getSenderPerson()!=null?message.getSenderPerson().getName():message.getSender();
            messages.put(new JSONObject().put("sender",text(sender,256)).put("text",text(message.getText(),1024)));
        }
        return messages;
    }
    /** RGBA8888 as hex: half the size of a JSON number array. */
    private static String pixels(Drawable drawable,int size) {
        Bitmap bitmap=Bitmap.createBitmap(size,size,Bitmap.Config.ARGB_8888);
        drawable.setBounds(0,0,size,size); drawable.draw(new Canvas(bitmap));
        int[] pixels=new int[size*size]; bitmap.getPixels(pixels,0,size,0,0,size,size); bitmap.recycle();
        StringBuilder out=new StringBuilder(size*size*8);
        for(int pixel:pixels) { hex(out,(pixel>>16)&255); hex(out,(pixel>>8)&255); hex(out,pixel&255); hex(out,(pixel>>>24)&255); }
        return out.toString();
    }
    private static void hex(StringBuilder out,int value) { out.append(Character.forDigit(value>>4,16)).append(Character.forDigit(value&15,16)); }
    private static String text(CharSequence source,int max) {
        if(source==null) return "";
        String value=source.toString().replace("\u0000","");
        if(value.length()<=max) return value;
        int end=max; if(Character.isHighSurrogate(value.charAt(end-1))) end--;
        return value.substring(0,end);
    }
    private void command(JSONObject message) {
        try {
            if("media".equals(message.optString("type"))) { mediaCommand(message); return; }
            Entry entry;
            synchronized(lock) { entry=active.get(message.getString("key")); }
            if(entry==null || !entry.version.equals(message.getString("version"))) return;
            Notification n=entry.sbn.getNotification();
            String type=message.getString("type");
            if(type.equals("dismiss")) { if(entry.sbn.isClearable()) cancelNotification(entry.sbn.getKey()); return; }
            if(!type.equals("action")) return;
            String action=message.getString("action"); PendingIntent intent=null;
            if(action.startsWith("x-denial-")) { control(entry,action); return; }
            if(action.equals("default")) intent=n.contentIntent;
            else if(action.startsWith("action-") && n.actions!=null) {
                int index=Integer.parseInt(action.substring(7));
                if(index>=0 && index<n.actions.length && n.actions[index].getRemoteInputs()==null) intent=n.actions[index].actionIntent;
            }
            ActivityOptions options=ActivityOptions.makeBasic().setPendingIntentBackgroundActivityStartMode(ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOWED);
            if(intent==null) {
                Intent launch=action.equals("default")?launchIntent(entry.sbn):null;
                if(launch!=null) context.startActivityAsUser(launch,options.toBundle(),entry.sbn.getUser());
                return;
            }
            intent.send(context,0,null,null,null,null,options.toBundle());
            if(action.equals("default") && (n.flags & Notification.FLAG_AUTO_CANCEL)!=0 && entry.sbn.isClearable()) cancelNotification(entry.sbn.getKey());
        } catch(Exception e) { Log.w(TAG,"Notification action unavailable: "+e.getClass().getSimpleName()); }
    }
    private Intent launchIntent(StatusBarNotification sbn) {
        Intent launch=context.getPackageManager().getLaunchIntentForPackage(sbn.getPackageName());
        if(launch!=null) launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK|Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED);
        return launch;
    }
    private static void send(DataOutputStream out,JSONObject message) throws Exception {
        byte[] bytes=message.toString().getBytes(StandardCharsets.UTF_8);
        if(bytes.length>MAX_FRAME) throw new IllegalArgumentException("Notification frame too large");
        out.writeInt(bytes.length); out.write(bytes); out.flush();
    }
    private static void closeSocket(HostChannel socket) {
        try { socket.shutdownInput(); } catch(Exception ignored) { }
        try { socket.shutdownOutput(); } catch(Exception ignored) { }
        try { socket.close(); } catch(Exception ignored) { }
    }
    private void disconnect() { connected=false; HostChannel current=socket; if(current!=null) closeSocket(current); synchronized(lock) { lock.notifyAll(); } }
    @Override public void close() { stopped=true; disconnect(); synchronized(lock) { forgetAll(); } try { unregisterAsSystemService(); } catch(Exception ignored) { } }
    public void dump(PrintWriter out) { synchronized(lock) { out.println("DROIDLOOM_NOTIFICATIONS_ABI=4; notificationsListening="+listening+" notificationsConnected="+connected+" activeNotifications="+active.size()); } }
}
