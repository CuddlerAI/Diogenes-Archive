#pragma once

#include "game_adapter.hpp"

#include <array>
#include <string>

namespace diogenes::gta {

struct StoryProjectionRequest {
    std::string output_path;
    std::array<double, 3> camera_position{};
    std::array<double, 3> camera_euler_deg{};
    std::array<double, 3> point_world{};
    double fov_deg{};
    double nominal_focal_mm{};
    bool draw_marker{true};
};

// Synchronous ScriptHook Story fiber only. Caller excludes active exports,
// Editor, and controller restarts. No GTA natives are invoked from Present.
// At most ten seconds of cooperative waits, including a bounded failure drain.
// Result is a draft Story observation, never an Editor frame/fence or approval.
AdapterResult capture_story_projection(const StoryProjectionRequest& request);

}  // namespace diogenes::gta
