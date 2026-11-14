// Codec2 AIDL IComponentStore/default on the host's V4L2 stateful video
// decoders (external/v4l2_codec2's store), for hardware video decoding.
//
// The upstream v4l2_codec2 service registers over HIDL, which Android 17
// leaves off (hwservicemanager disabled, media.c2.hal.selection=aidl). This
// is frameworks/av's sample vendor service with that store, AIDL only.
// No minijail: the cell is the boundary, as for Droidloom's other HALs.

#define LOG_TAG "android.hardware.media.c2-service.droidloom-v4l2"

#include <android-base/logging.h>
#include <android/binder_manager.h>
#include <android/binder_process.h>
#include <codec2/aidl/ComponentStore.h>
#include <v4l2_codec2/v4l2/V4L2ComponentStore.h>

#include <csignal>
#include <string>

int main(int /* argc */, char** /* argv */) {
    using namespace ::aidl::android::hardware::media::c2;

    android::base::InitLogging(nullptr, android::base::LogdLogger(android::base::SYSTEM));
    signal(SIGPIPE, SIG_IGN);

    // Codec2 calls can nest binder transactions: leave room for them.
    ABinderProcess_setThreadPoolMaxThreadCount(16);
    ABinderProcess_startThreadPool();

    std::shared_ptr<C2ComponentStore> v4l2Store = android::V4L2ComponentStore::Create();
    if (v4l2Store == nullptr) {
        LOG(ERROR) << "No V4L2 component store";
        return 1;
    }
    std::shared_ptr<IComponentStore> store =
            ::ndk::SharedRefBase::make<utils::ComponentStore>(v4l2Store);
    const std::string instance = std::string(IComponentStore::descriptor) + "/default";
    if (AServiceManager_addService(store->asBinder().get(), instance.c_str()) != EX_NONE) {
        LOG(ERROR) << "Cannot register " << instance;
        return 1;
    }
    LOG(INFO) << instance << " registered";
    ABinderProcess_joinThreadPool();
    return 0;
}
