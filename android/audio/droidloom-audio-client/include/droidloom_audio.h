// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once
#include <stdint.h>
#ifdef __cplusplus
#include <atomic>
extern "C" {
#endif
struct dl_audio_position {
    uint64_t frames, monotonic_ns, latency_ns, discontinuities;
};
enum dl_audio_direction { DL_AUDIO_PLAYBACK = 1, DL_AUDIO_CAPTURE = 2 };
enum dl_audio_command {
    DL_AUDIO_START = 1, DL_AUDIO_PAUSE = 2, DL_AUDIO_FLUSH = 3,
    DL_AUDIO_DRAIN = 4, DL_AUDIO_STANDBY = 5
};
// Each handle belongs to one HAL worker. An acquired pointer remains valid only
// until submit/close; capture pointers are read-only. Control requires no credit.
int32_t dl_audio_open(const char *path, uint32_t direction, uint32_t rate,
    uint32_t channels, uint32_t frames, void **output);
// The kind of sound a playback stream carries; the host gives each its own
// volume. dl_audio_open opens media.
enum dl_audio_role {
    DL_AUDIO_MEDIA = 0, DL_AUDIO_NOTIFICATION = 1, DL_AUDIO_RINGTONE = 2,
    DL_AUDIO_ALARM = 3, DL_AUDIO_CALL = 4
};
int32_t dl_audio_open_role(const char *path, uint32_t direction, uint32_t rate,
    uint32_t channels, uint32_t frames, uint32_t role, void **output);
void dl_audio_close(void *handle);
int32_t dl_audio_acquire(void *handle, uint32_t requested, void **pointer, uint32_t *frames);
int32_t dl_audio_submit(void *handle, uint32_t frames);
int32_t dl_audio_control(void *handle, uint64_t command);
int32_t dl_audio_position(void *handle, struct dl_audio_position *output);
#ifdef __cplusplus
}
namespace droidloom_audio {
// One default module owns these controls. They affect only Droidloom samples.
inline std::atomic<bool> mic_mute{false};
inline std::atomic<bool> master_mute{false};
inline std::atomic<float> master_volume{1.0f};
}
#endif
