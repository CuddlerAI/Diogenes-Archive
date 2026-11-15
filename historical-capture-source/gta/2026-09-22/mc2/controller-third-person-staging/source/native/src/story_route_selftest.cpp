#include "story_route.hpp"

#include <iostream>
#include <limits>

int main() {
    using namespace diogenes::gta;
    const std::array<double, 3> start{324.0, 168.0, 103.640823};
    const std::array<std::array<double, 3>, 4> expected{{
        {324.0, 183.0, 103.640823}, {309.0, 168.0, 103.640823},
        {324.0, 153.0, 103.640823}, {339.0, 168.0, 103.640823}}};
    std::array<double, 3> target{};
    for (std::size_t quadrant = 0; quadrant < expected.size(); ++quadrant) {
        if (!story_route_target(start, static_cast<double>(quadrant) * 90.0,
                                15.0, target)) return 1;
        for (std::size_t axis = 0; axis < target.size(); ++axis) {
            if (std::abs(target[axis] - expected[quadrant][axis]) > 1e-9) return 2;
        }
    }
    // POC059: the ped drifted to 175 degrees during warmup. A 90-degree
    // scenario must still target (309,168), not the observed forward vector.
    std::array<double, 3> drifted{};
    if (!story_route_target(start, 175.0, 15.0, drifted) ||
        !story_route_target(start, 90.0, 15.0, target) ||
        std::abs(target[1] - drifted[1]) < 14.0) return 3;
    const auto nan = std::numeric_limits<double>::quiet_NaN();
    const auto inf = std::numeric_limits<double>::infinity();
    for (const auto bad_heading : {-1.0, 360.0, nan, inf}) {
        if (story_route_target(start, bad_heading, 15.0, target)) return 4;
    }
    for (const auto bad_distance : {0.0, -1.0, nan, inf}) {
        if (story_route_target(start, 90.0, bad_distance, target)) return 5;
    }
    if (story_route_target({nan, 0.0, 0.0}, 90.0, 15.0, target)) return 6;
    if (!story_anchor_matches(start, start) ||
        !story_anchor_matches({0.0, 0.0, 0.0}, {0.25, -0.25, 0.25}) ||
        story_anchor_matches({324.0, 168.0, 103.0}, start) ||
        story_anchor_matches(start, {nan, 168.0, 103.640823}) ||
        story_anchor_matches({inf, 168.0, 103.640823}, start)) return 7;
    std::cout << "story route heading and settled-anchor contract passed\n";
    return 0;
}
