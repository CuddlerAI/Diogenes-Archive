// Host-buildable half of the camera-tape.bin cross-language contract test.
// The Python writer commits tests/fixtures/golden-camera-tape.{json,bin}; this
// binary must decode the .bin to exactly the values the JSON declares. Any
// layout drift on either side turns this red in CI instead of on a capture rig.
#include "native_tape.hpp"

#include <cstdio>
#include <cstdint>
#include <string>

namespace {

int fail(const char* message) {
    std::fprintf(stderr, "native tape golden mismatch: %s\n", message);
    return 1;
}

}  // namespace

int main(int argc, char** argv) {
    if (argc != 3) {
        std::fprintf(stderr, "usage: %s <golden-camera-tape.bin> <golden-json-sha256>\n",
                     argv[0]);
        return 2;
    }
    diogenes::gta::NativeTape tape;
    std::string error;
    if (!tape.load(argv[1], argv[2], error)) {
        std::fprintf(stderr, "golden tape load failed: %s\n", error.c_str());
        return 1;
    }
    if (tape.fps_num() != 30 || tape.fps_den() != 1) return fail("fps");
    if (tape.json_sha256() != std::string(argv[2])) return fail("json sha256");

    constexpr int kFrames = 3;
    constexpr std::uint64_t kSourceTNs[kFrames] = {0, 33333333, 66666667};
    constexpr int kFocalByLane[5] = {0, 18, 24, 35, 50};
    for (int lane_id = 1; lane_id <= 4; ++lane_id) {
        const auto* lane = tape.lane(lane_id);
        if (lane == nullptr) return fail("missing lane");
        if (lane->focal_mm != kFocalByLane[lane_id]) return fail("focal_mm");
        if (lane->width != 1920 || lane->height != 1080) return fail("resolution");
        if (lane->samples.size() != kFrames) return fail("sample count");
        for (int frame = 0; frame < kFrames; ++frame) {
            const auto& sample = lane->samples[static_cast<std::size_t>(frame)];
            // The fixture uses exactly representable doubles on purpose, so the
            // contract check is exact equality, not tolerance.
            if (sample.frame != static_cast<std::uint32_t>(frame)) return fail("frame index");
            if (sample.source_t_ns != kSourceTNs[frame]) return fail("source_t_ns");
            if (sample.position[0] != static_cast<double>(lane_id) ||
                sample.position[1] != static_cast<double>(frame) ||
                sample.position[2] != 0.5) return fail("position");
            if (sample.quaternion[0] != 0.0 || sample.quaternion[1] != 0.0 ||
                sample.quaternion[2] != 0.0 || sample.quaternion[3] != 1.0)
                return fail("quaternion");
            if (sample.euler_deg[0] != 0.0 || sample.euler_deg[1] != 0.0 ||
                sample.euler_deg[2] != 0.0) return fail("euler_deg");
            if (sample.intrinsics[0] != 1000.0 + lane_id ||
                sample.intrinsics[1] != 1000.0 + lane_id ||
                sample.intrinsics[2] != 960.0 ||
                sample.intrinsics[3] != 540.0) return fail("intrinsics");
        }
    }

    // The digest binding must also fail closed on a wrong sealed-JSON hash.
    diogenes::gta::NativeTape mismatched;
    const std::string wrong(64, '0');
    if (mismatched.load(argv[1], wrong, error)) {
        return fail("wrong sealed-JSON sha256 was accepted");
    }
    std::printf("native tape golden contract ok\n");
    return 0;
}
