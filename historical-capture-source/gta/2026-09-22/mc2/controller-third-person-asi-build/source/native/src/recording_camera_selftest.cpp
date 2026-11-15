#include "recording_camera.hpp"

#include <iostream>
#include <string>
#include <vector>

int main() {
    using namespace diogenes::gta;
    // A setter returning normally is not proof that GTA accepted the camera.
    std::string error;
    int selected = -1;
    int waits = 0;
    int reads = 0;
    if (prepare_recording_camera(
            [&](int mode) { selected = mode; },
            [&] { ++waits; return true; },
            [&] { ++reads; return RecordingCameraObservation{true, 4}; }, error) ||
        selected != 1 || waits != 60 || reads != 60 || error.empty()) return 1;

    // A transient third-person result must not end settling; require two
    // consecutive game-frame observations after the final transition.
    const std::vector<RecordingCameraObservation> delayed{
        {true, 4}, {true, 1}, {false, 1}, {true, 1}, {true, 1}};
    waits = 0;
    reads = 0;
    if (!prepare_recording_camera(
            [](int) {}, [&] { ++waits; return true; },
            [&] { return delayed.at(static_cast<std::size_t>(reads++)); }, error) ||
        waits != 5 || reads != 5 || !error.empty()) return 2;

    // Losing Story Mode must stop before another observation/native read.
    reads = 0;
    if (prepare_recording_camera(
            [](int) {}, [] { return false; },
            [&] { ++reads; return RecordingCameraObservation{true, 1}; }, error) ||
        reads != 0 || error.empty()) return 3;

    // A saved mode flag while another camera is active, unknown/cinematic modes,
    // and any drift away from the selected mode are not recording permission.
    for (const int mode : {-1, 0, 2, 3, 4, 5}) {
        if (recording_camera_matches({true, mode})) return 4;
    }
    if (recording_camera_matches({false, 1}) ||
        !recording_camera_matches({true, 1})) return 5;
    std::cout << "recording camera settle, Story Mode loss and drift contract passed\n";
    return 0;
}
