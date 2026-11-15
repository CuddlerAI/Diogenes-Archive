#pragma once

#include <string>

namespace diogenes::gta {

// GTA's follow-ped view enum: 0 near, 1 medium, 2 far, 3 cinematic, 4 first person.
// Fix one third-person mode before recording; a mode flag is not pixel visibility.
constexpr int kRecordingFollowPedViewMode = 1;

struct RecordingCameraObservation {
    bool follow_ped_active{false};
    int view_mode{-1};
};

inline bool recording_camera_matches(const RecordingCameraObservation& observed) {
    return observed.follow_ped_active &&
           observed.view_mode == kRecordingFollowPedViewMode;
}

// The callbacks keep the game API at the adapter boundary and let the actual
// settling/failure contract be tested without starting GTA. The caller checks
// Story Mode before entry; every wait must check it again before reading natives.
template <typename SelectMode, typename WaitFrame, typename Observe>
bool prepare_recording_camera(SelectMode select_mode, WaitFrame wait_frame,
                              Observe observe, std::string& error) {
    select_mode(kRecordingFollowPedViewMode);
    int consecutive = 0;
    constexpr int kMaxSettleFrames = 60;
    for (int frame = 0; frame < kMaxSettleFrames; ++frame) {
        if (!wait_frame()) {
            error = "Story Mode ended while preparing the third-person recording camera";
            return false;
        }
        consecutive = recording_camera_matches(observe()) ? consecutive + 1 : 0;
        if (consecutive == 2) {
            error.clear();
            return true;
        }
    }
    error = "third-person follow-ped camera did not settle before recording";
    return false;
}

}  // namespace diogenes::gta
