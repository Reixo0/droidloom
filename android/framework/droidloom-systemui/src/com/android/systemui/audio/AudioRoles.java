/* Copyright 2026 Droidloom. SPDX-License-Identifier: GPL-3.0-or-later */
package com.android.systemui.audio;

import android.content.Context;
import android.media.AudioAttributes;
import android.media.AudioDeviceCallback;
import android.media.AudioDeviceInfo;
import android.media.AudioFormat;
import android.media.AudioManager;
import android.media.audiopolicy.AudioMix;
import android.media.audiopolicy.AudioMixingRule;
import android.media.audiopolicy.AudioPolicy;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;
import java.io.PrintWriter;

/**
 * Sends each kind of Android sound to its own BUS output, as Android Automotive's
 * CarAudioService does: the HAL names each host stream by its BUS address, and the
 * host gives each kind a volume of its own. Whatever no mix takes stays on the
 * default output, media.
 */
public final class AudioRoles {
    private static final String TAG = "DroidloomAudioRoles";

    private static final class Route {
        final String address;
        final int[] usages;

        Route(String address, int... usages) {
            this.address = address;
            this.usages = usages;
        }
    }

    private static final Route[] ROUTES = {
        new Route("droidloom-host-notification",
                AudioAttributes.USAGE_NOTIFICATION,
                AudioAttributes.USAGE_NOTIFICATION_EVENT,
                AudioAttributes.USAGE_ASSISTANCE_SONIFICATION),
        new Route("droidloom-host-ringtone", AudioAttributes.USAGE_NOTIFICATION_RINGTONE),
        new Route("droidloom-host-alarm", AudioAttributes.USAGE_ALARM),
        new Route("droidloom-host-call",
                AudioAttributes.USAGE_VOICE_COMMUNICATION,
                AudioAttributes.USAGE_VOICE_COMMUNICATION_SIGNALLING),
    };

    private final Context context;
    private final AudioManager audio;
    private final Handler handler = new Handler(Looper.getMainLooper());
    private AudioPolicy policy;
    private String state = "not started";

    /** Retries once the BUS outputs show, should audioserver list them late. */
    private final AudioDeviceCallback devices = new AudioDeviceCallback() {
        @Override
        public void onAudioDevicesAdded(AudioDeviceInfo[] added) {
            if (policy == null) register();
        }
    };

    public AudioRoles(Context context) {
        this.context = context;
        this.audio = context.getSystemService(AudioManager.class);
    }

    public void start() {
        audio.registerAudioDeviceCallback(devices, handler);
        register();
    }

    public void close() {
        audio.unregisterAudioDeviceCallback(devices);
        if (policy != null) {
            audio.unregisterAudioPolicy(policy);
            policy = null;
        }
        state = "closed";
    }

    private void register() {
        AudioPolicy.Builder builder = new AudioPolicy.Builder(context);
        AudioFormat format = new AudioFormat.Builder()
                .setSampleRate(48000)
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                .build();
        for (Route route : ROUTES) {
            AudioDeviceInfo device = find(route.address);
            if (device == null) {
                // All or none: a usage without its output would sound on media.
                state = "waiting for " + route.address;
                return;
            }
            AudioMixingRule.Builder rule = new AudioMixingRule.Builder();
            for (int usage : route.usages) {
                rule.addRule(new AudioAttributes.Builder().setUsage(usage).build(),
                        AudioMixingRule.RULE_MATCH_ATTRIBUTE_USAGE);
            }
            builder.addMix(new AudioMix.Builder(rule.build())
                    .setFormat(format)
                    .setRouteFlags(AudioMix.ROUTE_FLAG_RENDER)
                    .setDevice(device)
                    .build());
        }
        AudioPolicy built = builder.build();
        int status = audio.registerAudioPolicy(built);
        if (status == AudioManager.SUCCESS) {
            policy = built;
            state = "routing " + ROUTES.length + " kinds of sound";
            Log.i(TAG, state);
        } else {
            state = "registration failed: " + status;
            Log.w(TAG, state);
        }
    }

    private AudioDeviceInfo find(String address) {
        for (AudioDeviceInfo device : audio.getDevices(AudioManager.GET_DEVICES_OUTPUTS)) {
            if (device.getType() == AudioDeviceInfo.TYPE_BUS
                    && address.equals(device.getAddress())) {
                return device;
            }
        }
        return null;
    }

    public void dump(PrintWriter out) {
        out.println("audioRoles=" + state);
    }
}
