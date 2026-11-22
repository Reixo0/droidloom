package com.android.systemui.notifications;

import android.app.ActivityManager;
import android.app.IForegroundServiceObserver;
import android.app.job.IUserVisibleJobObserver;
import android.app.job.JobScheduler;
import android.app.job.UserVisibleJobSummary;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.IBinder;
import android.os.PowerExemptionManager;
import android.os.Process;
import android.util.Log;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.Map;
import org.json.JSONArray;
import org.json.JSONObject;

/**
 * The apps Android's task manager lists (SystemUI's FgsManagerController): those running a
 * foreground service or a user-visible job, without the system's own, and whether the user may
 * stop them. Only the system user's.
 */
final class ActiveApps {
    private static final String TAG = "DroidloomActiveApps";
    private static final int MAX = 64;

    private static final class App {
        final HashSet<IBinder> services = new HashSet<>();
        final HashSet<UserVisibleJobSummary> jobs = new HashSet<>();
        final long since = System.currentTimeMillis();
        String label = "", component = "";
        boolean hidden, stoppable;
    }

    private final Context context;
    private final Runnable changed;
    private final Handler worker;
    /** Under this object's lock. */
    private final LinkedHashMap<String,App> apps = new LinkedHashMap<>();

    ActiveApps(Context context, Runnable changed) {
        this.context = context; this.changed = changed;
        HandlerThread thread = new HandlerThread(TAG, Process.THREAD_PRIORITY_BACKGROUND);
        thread.start();
        worker = new Handler(thread.getLooper());
    }

    void start() {
        try {
            ActivityManager.getService().registerForegroundServiceObserver(new IForegroundServiceObserver.Stub() {
                @Override public void onForegroundStateChanged(IBinder token, String pkg, int user, boolean foreground) {
                    worker.post(() -> service(token, pkg, user, foreground));
                }
            });
            JobScheduler jobs = context.getSystemService(JobScheduler.class);
            if (jobs != null) jobs.registerUserVisibleJobObserver(new IUserVisibleJobObserver.Stub() {
                @Override public void onUserVisibleJobStateChanged(UserVisibleJobSummary summary, boolean running) {
                    worker.post(() -> job(summary, running));
                }
            });
        } catch (Exception e) { Log.w(TAG, "Active apps unavailable: " + e.getClass().getSimpleName()); }
    }

    private void service(IBinder token, String pkg, int user, boolean foreground) {
        if (user != 0) return;
        if (foreground) {
            App app = app(pkg);
            synchronized (this) { if (!app.services.add(token)) return; }
        } else {
            synchronized (this) {
                App app = apps.get(pkg);
                if (app == null || !app.services.remove(token)) return;
                retire(pkg, app);
            }
        }
        changed.run();
    }

    private void job(UserVisibleJobSummary summary, boolean running) {
        if (summary.getSourceUserId() != 0) return;
        String pkg = summary.getSourcePackageName();
        if (running) {
            App app = app(pkg);
            synchronized (this) { if (!app.jobs.add(summary)) return; }
        } else {
            synchronized (this) {
                App app = apps.get(pkg);
                if (app == null || !app.jobs.remove(summary)) return;
                retire(pkg, app);
            }
        }
        changed.run();
    }

    private void retire(String pkg, App app) {
        if (app.services.isEmpty() && app.jobs.isEmpty()) apps.remove(pkg);
    }

    /** The app's entry, made with what its package says. On the worker. */
    private App app(String pkg) {
        synchronized (this) { App app = apps.get(pkg); if (app != null) return app; }
        App app = new App();
        PackageManager pm = context.getPackageManager();
        app.label = pkg;
        try {
            app.label = String.valueOf(pm.getApplicationLabel(pm.getApplicationInfo(pkg, 0)));
            Intent launch = pm.getLaunchIntentForPackage(pkg);
            if (launch != null && launch.getComponent() != null) app.component = launch.getComponent().flattenToString();
            switch (ActivityManager.getService().getBackgroundRestrictionExemptionReason(pm.getPackageUid(pkg, 0))) {
                case PowerExemptionManager.REASON_SYSTEM_UID:
                case PowerExemptionManager.REASON_DEVICE_DEMO_MODE:
                    app.hidden = true; break;
                case PowerExemptionManager.REASON_SYSTEM_ALLOW_LISTED:
                case PowerExemptionManager.REASON_DEVICE_DPC:
                case PowerExemptionManager.REASON_DISALLOW_APPS_CONTROL:
                case PowerExemptionManager.REASON_DPO_PROTECTED_APP:
                case PowerExemptionManager.REASON_USER_DPC:
                case PowerExemptionManager.REASON_ACTIVE_DEVICE_ADMIN:
                case PowerExemptionManager.REASON_PROC_STATE_PERSISTENT:
                case PowerExemptionManager.REASON_PROC_STATE_PERSISTENT_UI:
                case PowerExemptionManager.REASON_ROLE_DIALER:
                case PowerExemptionManager.REASON_SYSTEM_MODULE:
                case PowerExemptionManager.REASON_SYSTEM_EXEMPT_APP_OP:
                    break;
                default:
                    app.stoppable = true;
            }
        } catch (Exception e) { Log.w(TAG, "Active app not described: " + e.getClass().getSimpleName()); }
        synchronized (this) { apps.put(pkg, app); }
        return app;
    }

    synchronized JSONArray list() throws org.json.JSONException {
        JSONArray list = new JSONArray();
        for (Map.Entry<String,App> entry : apps.entrySet()) {
            App app = entry.getValue();
            if (app.hidden || list.length() >= MAX) continue;
            list.put(new JSONObject().put("package", entry.getKey())
                    .put("label", app.label.length() > 256 ? app.label.substring(0, 256) : app.label)
                    .put("component", app.component).put("since", app.since).put("stoppable", app.stoppable));
        }
        return list;
    }

    /** Stops an app of the list, as the task manager's Stop button. */
    void stop(String pkg) {
        synchronized (this) {
            App app = apps.get(pkg);
            if (app == null || app.hidden || !app.stoppable) return;
        }
        worker.post(() -> {
            try {
                JobScheduler jobs = context.getSystemService(JobScheduler.class);
                if (jobs != null) jobs.notePendingUserRequestedAppStop(pkg, 0, "task manager");
                ActivityManager.getService().stopAppForUser(pkg, 0);
            } catch (Exception e) { Log.w(TAG, "Active app not stopped: " + e.getClass().getSimpleName()); }
        });
    }
}
