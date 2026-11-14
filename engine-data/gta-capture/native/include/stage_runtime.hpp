#pragma once

// Stage runtime: the game side of stage_plan. Loads the clips and models a spec
// names, measures the clips (calibration), spawns the cast and plays a resolved
// plan tick by tick. Script-fiber only: every call may run natives, and the ones
// that take frames advance them through the host's frame hook, so the host's
// per-frame policy (population, HUD, camera) keeps running throughout.

#include "stage_plan.hpp"

#include <array>
#include <functional>
#include <string>
#include <utility>
#include <vector>

namespace diogenes::gta::stage {

constexpr int kSampleBones = 7;   // ROOT, Spine3, Head, L_Hand, R_Hand, L_Foot, R_Foot
extern const std::array<int, kSampleBones> kSampleBoneIds;
extern const std::array<const char*, kSampleBones> kSampleBoneNames;

struct Hooks {
    std::function<void()> frame;                   // advance exactly one game frame (host policy + WAIT(0))
    std::function<double()> now_s;                 // monotonic wall clock
    std::function<void(const std::string&)> log;   // one line of the take's log
    std::function<long long()> game_ms;            // the game clock (GET_GAME_TIMER), which the replay plays on
};

struct EntitySample {
    bool present = false;
    float pos[3] = {0, 0, 0};
    float heading = 0;
    float phase = -1;
    float bone[kSampleBones][3] = {};
};
// A tracked thing of the map's (StageSpec::tracks): where it is and how it is turned, whole (a ferris wheel turns about
// its axle, not about the up axis).
struct MapSample {
    bool present = false;
    float pos[3] = {0, 0, 0};
    float quat[4] = {0, 0, 0, 1};   // x y z w, GET_ENTITY_QUATERNION
};

class Runtime {
public:
    explicit Runtime(Hooks hooks) : hooks_(std::move(hooks)) {}
    ~Runtime();
    Runtime(const Runtime&) = delete;
    Runtime& operator=(const Runtime&) = delete;

    // Streams in every clip dictionary, ped model and prop model the spec names, and
    // for each set piece the first of its models the game has. Returns "" or which
    // asset failed; a clip whose duration reads 0 has failed.
    std::string load_assets(const StageSpec& spec);

    // Measures every clip on the model of an actor that plays it: duration, the
    // root offset and heading at every phase, the pose at every phase, and how
    // high its origin must sit for the feet to be on the sand -- against a ped
    // standing naturally on the same sand, read only once it really stands. Then
    // every seat: where the game itself seats a person on the piece (SeatCal).
    std::string calibrate(const StageSpec& spec, Calibration& out);

    // Ground height under (x, y), probed from 5 m above the stage's height.
    double ground(double x, double y);
    int ground_misses() const { return ground_misses_; }

    // Puts the set pieces where the plan puts them, then spawns the cast where tick 0
    // puts it: models, pinned looks, hand props.
    std::string spawn(const StageSpec& spec, const Plan& plan, const Calibration& cal);

    // Tick k: enter() puts every actor whose beat starts on k into its new scene --
    // created at k's origin and phase, so the frames the task takes to hold already
    // show k's pose -- and returns how many did; pose() then sets every origin and
    // phase for k. verify() checks the new scenes entered on tick `upto` or before
    // that it has not checked yet, once their tasks have had the frames they take
    // to hold, and stops each one's own clock: the phase comes from pose() alone.
    int enter(const Plan& plan, const Calibration& cal, int k);
    std::string verify(const Plan& plan, int upto);
    void pose(const Plan& plan, const Calibration& cal, int k);
    void retire_due(int k);

    // Takes an actor out of the plan from tick k on (a takeover or a rule). It is
    // cleared of its scene -- at once, or (gently) left to blend out of it as the task
    // the caller gives it takes over -- and the caller hands it to the player or gives
    // it a task.
    void release(int actor, int k, bool gently = false);
    bool released(int actor) const;
    int released_from(int actor) const;
    // Puts a released actor back into the plan `delay` ticks behind it: from then on it
    // plays the plan's tick k - delay, so it picks up in the beat and at the phase it
    // left (a watcher back at its own thing, stage_take). Its scene blends in.
    void rejoin(int actor, int delay);
    int delay(int actor) const;

    int actors() const { return static_cast<int>(cast_.size()); }
    int ped(int actor) const;
    const std::string& actor_id(int actor) const;
    int props() const { return static_cast<int>(props_.size()); }
    std::string prop_id(int i) const;
    int prop_owner(int i) const;
    int tracks() const { return static_cast<int>(tracks_.size()); }
    const std::string& track_id(int i) const { return tracks_[static_cast<size_t>(i)].id; }
    void sample_track(int i, MapSample& out) const;
    // The map's objects spawn() removed for the spec's clear lines, and how many of them are there now (0 while the
    // clears hold; counted as a take ends, before despawn() lifts them).
    int cleared() const { return cleared_; }
    int cleared_left() const;
    // Every object of the game's object pool within `radius` metres of world (x, y), one JSON line each: its model hash,
    // where it is, its quaternion -- what a track line names (the take writes it as it begins and as it ends, so what
    // turns or moves shows).
    static std::string scan_objects(double x, double y, double radius);

    void sample_actor(int actor, EntitySample& out) const;
    void sample_prop(int i, EntitySample& out) const;
    static void sample_ped(int ped, EntitySample& out);

    int scenes_created() const { return scenes_created_; }
    int scene_failures() const { return scene_failures_; }
    int distinct_scene_ids() const { return static_cast<int>(scene_ids_.size()); }
    int entries_checked() const { return entries_checked_; }
    int entries_not_playing() const { return entries_not_playing_; }
    int entries_not_running() const { return entries_not_running_; }
    // actors the runtime let go of itself (the scene pool refused them), not a rule or a takeover
    int runtime_releases() const { return runtime_releases_; }

    // Deletes the cast and props and retires every scene exactly once.
    void despawn();
    // Lets the game stream out the clips and models load_assets() asked for.
    void release_assets();

private:
    struct Actor {
        int ped = 0;
        int scene = -1;
        int beat = -1;
        int released_from = -1;
        int delay = 0;            // ticks it plays behind the plan (rejoin)
        bool rejoining = false;   // its next scene blends in from wherever it stands
    };
    struct Prop { int object = 0; int owner = -1; std::string model; bool shown = true; };
    struct Track { std::string id; unsigned model = 0; int object = 0; };   // the map's own: found, never deleted
    struct Clear {   // world coords; the model hidden there (boats: the generators there off, the models suppressed)
        unsigned model = 0;
        double x = 0, y = 0, z = 0, radius = 0;
        bool boats = false;
        std::vector<unsigned> suppressed;
    };
    struct Piece { std::string id, name; unsigned model = 0; bool vehicle = false; int entity = 0; };   // a set piece
    struct Shared { int pair = -1, start = -1, scene = -1; };   // a pair beat's one scene, both halves in it
    struct Entry { int tick = -1, actor = -1, scene = -1, beat = -1; };   // a new scene, not checked yet

    void step(int frames);
    int place(const Piece& piece, double x, double y, double z, double heading);
    void remove(Piece& piece);
    int new_scene(double x, double y, double z, double heading);
    void queue_retire(int scene, int due);
    bool scene_in_use(int scene) const;
    void retire_scene(int scene);
    std::string clip_dict(const std::string& clip_id) const;
    std::string clip_name(const std::string& clip_id) const;

    Hooks hooks_;
    StageSpec spec_;
    std::vector<Actor> cast_;
    std::vector<Prop> props_;
    std::vector<Track> tracks_;
    std::vector<Clear> clears_;
    int cleared_ = 0;
    std::vector<Piece> pieces_;                 // StageSpec::sets order: the model chosen and, once placed, the entity
    std::vector<std::pair<int, int>> retire_;   // (scene, due tick)
    std::vector<Shared> shared_;                // live pair scenes; a retired one leaves the list
    std::vector<int> scene_ids_;
    std::vector<Entry> entered_;                // new scenes verify() has still to check
    std::vector<std::string> dicts_;            // what load_assets() streamed in
    std::vector<unsigned> models_;
    double last_ground_ = 0;
    bool have_ground_ = false;
    int ground_misses_ = 0;
    int scenes_created_ = 0, scene_failures_ = 0;
    int entries_checked_ = 0, entries_not_playing_ = 0, entries_not_running_ = 0;
    int runtime_releases_ = 0;
};

}  // namespace diogenes::gta::stage
