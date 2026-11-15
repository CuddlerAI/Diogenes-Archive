#pragma once

#include <cstdint>
#include <memory>
#include <string>
#include <utility>

namespace diogenes::gta {

struct AdapterResult {
    bool ok{false};
    std::string payload_json{"{}"};
    std::string error;

    static AdapterResult success(std::string payload = "{}") {
        return AdapterResult{true, std::move(payload), {}};
    }

    static AdapterResult failure(std::string message) {
        return AdapterResult{false, "{}", std::move(message)};
    }
};

class GameAdapter {
public:
    virtual ~GameAdapter() = default;
    virtual void set_runtime_generation(std::uint64_t generation) = 0;
    virtual AdapterResult hello() = 0;
    virtual AdapterResult probe_state() = 0;
    // Independent Story calibration diagnostic; never an Editor frame seal.
    virtual AdapterResult story_projection_sample(const std::string& request_path,
                                                  const std::string& request_sha256,
                                                  const std::string& output_path) = 0;
    virtual AdapterResult apply_scenario(const std::string& scenario_path,
                                         const std::string& scenario_sha256) = 0;
    virtual AdapterResult record_master(const std::string& run_dir, double duration_s) = 0;
    virtual AdapterResult arm_export_lane(int lane, const std::string& tape_path,
                                          const std::string& tape_sha256,
                                          const std::string& replay_path,
                                          const std::string& replay_sha256,
                                          const std::string& run_dir,
                                          const std::string& expected_output_root,
                                          const std::string& render_profile_sha256) = 0;
    virtual AdapterResult export_lane(int lane, const std::string& tape_path,
                                      const std::string& tape_sha256,
                                      const std::string& replay_path,
                                      const std::string& replay_sha256,
                                      const std::string& run_dir,
                                      const std::string& expected_output_root,
                                      const std::string& render_profile_sha256) = 0;
    virtual AdapterResult activate_editor() = 0;
    virtual AdapterResult editor_input(int control) = 0;
    virtual AdapterResult probe_editor() = 0;
    virtual AdapterResult capture_audio(const std::string& replay_path,
                                        const std::string& replay_sha256,
                                        const std::string& run_dir,
                                        double duration_s) = 0;
    virtual void on_tick() = 0;
    // ScriptHookV keeps Present callbacks alive while Rockstar Editor suspends
    // script fibers. Implementations must never call GTA natives from here.
    virtual void on_present() = 0;
};

std::unique_ptr<GameAdapter> make_mock_game_adapter();

#ifdef DIOGENES_SCRIPT_HOOK_V_ADAPTER
std::unique_ptr<GameAdapter> make_scripthook_game_adapter();
#endif

}  // namespace diogenes::gta
