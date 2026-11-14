#pragma once

#include <cstdint>
#include <memory>
#include <string>
#include <utility>
#include <vector>

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

// One later clip of a multi-clip take: its sealed copy, digest and Rockstar name.
struct TakeClipRef {
    std::string replay_path;
    std::string replay_sha256;
    std::string source_clip_name;
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
    // The lanes of one take armed as a queue: the first at once, each next one
    // as soon as the lane before it is sealed, so the Editor is left once for all
    // of them. collect_export_lanes returns every lane's result, in order.
    virtual AdapterResult arm_export_lanes(const std::vector<int>& lanes, const std::string& tape_path,
                                           const std::string& tape_sha256,
                                           const std::string& replay_path,
                                           const std::string& replay_sha256,
                                           const std::string& run_dir,
                                           const std::string& expected_output_root,
                                           const std::string& render_profile_sha256) {
        (void)lanes; (void)tape_path; (void)tape_sha256; (void)replay_path; (void)replay_sha256;
        (void)run_dir; (void)expected_output_root; (void)render_profile_sha256;
        return AdapterResult::failure("a lane queue needs the game");
    }
    virtual AdapterResult collect_export_lanes(const std::string& run_dir) {
        (void)run_dir;
        return AdapterResult::failure("a lane queue needs the game");
    }
    virtual AdapterResult activate_editor() = 0;
    virtual AdapterResult editor_input(int control) = 0;
    virtual AdapterResult probe_editor() = 0;
    virtual AdapterResult capture_audio(const std::string& replay_path,
                                        const std::string& replay_sha256,
                                        const std::string& run_dir,
                                        double duration_s) = 0;
    // A look at a ped's clothes, for choosing a stage actor's `look` (2026-09-29: shoes
    // the tourist's photo clip does not sink): a ped of `model` stands 2.5 m in front of
    // the player, facing it, dressed in the model's defaults and then component/drawable/
    // texture, looping `dict`/`clip` if given; returns how many drawables each component
    // has and textures this drawable has, and how high the feet and toes are over the
    // ground. `clear` deletes it. Nothing is recorded.
    virtual AdapterResult ped_look(const std::string& model, int component, int drawable, int texture,
                                   const std::string& dict, const std::string& clip, bool clear) {
        (void)model; (void)component; (void)drawable; (void)texture; (void)dict; (void)clip; (void)clear;
        return AdapterResult::failure("a ped look needs the game");
    }
    // The ground's height on a grid round (x, y), half_m each way every step_m, z a height near the ground there
    // (the player waits there while the map loads). Nothing is recorded.
    virtual AdapterResult ground_scan(double x, double y, double z, double half_m, double step_m) {
        (void)x; (void)y; (void)z; (void)half_m; (void)step_m;
        return AdapterResult::failure("a ground scan needs the game");
    }
    // A rehearsal: the stage spec's cast plays its plan on the stage, from stage second
    // `from_s` for `seconds`, as a take would put it there -- the same assets, calibration
    // (measured, or the file given), plan and scenes -- with nothing recorded and the player
    // left to walk round it and look. Made to set the last centimetres of a clip's height
    // by eye (StageSpec lift) before anybody records. `watch`: an actor the player is put
    // `distance_m` from, facing it. The player goes to (x y z) first when it is not near.
    virtual AdapterResult stage_rehearse(const std::string& spec_path, const std::string& calibration_path,
                                         const std::string& calibration_out, const std::string& watch,
                                         double from_s, double seconds, double distance_m,
                                         double x, double y, double z) {
        (void)spec_path; (void)calibration_path; (void)calibration_out; (void)watch;
        (void)from_s; (void)seconds; (void)distance_m; (void)x; (void)y; (void)z;
        return AdapterResult::failure("a rehearsal needs the game");
    }
    // A few seconds' recording under each condition a take uses; which leave a clip.
    virtual AdapterResult recorder_selftest() {
        return AdapterResult::failure("the recorder self-test needs the game");
    }
    // Binds a take recorded by an earlier GTA process for export in this one,
    // proven by content: sealed copy, live Rockstar clip and result hash alike.
    // A take the recorder cut into several clips names the later ones in more_clips.
    virtual AdapterResult bind_take(const std::string& replay_path, const std::string& replay_sha256,
                                    const std::string& source_clip_name,
                                    const std::string& player_telemetry_path,
                                    const std::string& player_telemetry_sha256,
                                    const std::vector<TakeClipRef>& more_clips) {
        (void)replay_path; (void)replay_sha256; (void)source_clip_name;
        (void)player_telemetry_path; (void)player_telemetry_sha256; (void)more_clips;
        return AdapterResult::failure("binding a recorded take needs the game");
    }
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
