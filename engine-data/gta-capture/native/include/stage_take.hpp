#pragma once

// One take of a four-worldline group (issue #958): the cast on its resolved
// plan, the player in its role, and three logs written tick by tick on the
// game clock -- the clock the replay plays back on (rehearsals v3-v5: the
// clips hold exactly the game span; the wall clock runs ahead by every hitch).
//
//   AB  the player's body waits hidden out of every shot while a person flies a
//       free camera (worldline B's lane); the rest of the take is worldline A.
//   C   until tick N the hidden player rides along on the actor to be taken
//       over, so the game camera already follows that person; at N the player
//       takes the actor's body and the actor leaves the plan.
//   D   a seventh person is spawned and the player plays it from tick 0. The
//       player's own story character is never re-modelled: it waits hidden.
//
// Logs (each line one tick, JSON):
//   master-world-telemetry.jsonl  dm-gta-world-telemetry/1: every actor, prop and the player
//   input-camera-log.jsonl        dm-gta-input-camera-log/1: keys held, mouse look, the
//                                 cameras (game, free), the controlled person's head
//   rule-events.jsonl             dm-gta-rule-events/1: takeover, seventh person, every
//                                 rule that fired, with its Chinese causal record
// Mouse deltas are the game's look-axis readings of the physical mouse,
// summed over the tick's frames -- read, never derived from a camera path.

#include "stage_robot.hpp"
#include "stage_runtime.hpp"

#include <algorithm>
#include <cmath>
#include <fstream>
#include <map>
#include <memory>
#include <set>
#include <string>
#include <vector>

namespace diogenes::gta::stage {

enum class TakeKind { AB, C, D };

struct TakeConfig {
    TakeKind kind = TakeKind::AB;
    // AB: where the free camera starts (stage coordinates, height above the ground)
    double cam_x = 0, cam_y = -8.0, cam_h = 1.7, cam_heading = 0, cam_fov = 50.0;
    // C: who is taken over, and on which tick; the route the program then walks, if
    // one is given ("from_s x y stand_s look; ...", stage_robot.hpp) -- else it wanders
    std::string takeover_actor;
    int takeover_tick = -1;
    std::string route;
    // C/D: how the program turns the standing body (stage_robot.hpp parse_turn): "" or "game" turns it the
    // game's way in one go, "steps" a step of the game's turn at a time, "glide <deg/s> <deg/s^2>" by its
    // heading every frame (the customer, 2026-10-10: turning on the spot was too fast)
    std::string turn;
    // D: the seventh person (stage coordinates)
    std::string seventh_model;
    double seventh_x = 0, seventh_y = -6.0, seventh_heading = 0;
    // where the player's own body waits, hidden, out of every shot (stage coordinates)
    double park_x = 0, park_y = -40.0;
};

// One axis of the free camera: a position (or angle), its rate, and the rate's rate.
struct FlightAxis { double x = 0, v = 0, a = 0; };

// Steps an axis by dt toward a target rate within the limits: the rate by
// max_rate, its change by max_acc, the change of that by max_jerk -- including
// on the way in: the acceleration is only ever as large as the jerk limit can
// still take back to zero as the gap closes (a = sqrt(2 j |gap|)), so the rate
// eases onto its target instead of stopping dead. One jerk holds over the whole
// step and the step is integrated exactly, so the path is the true cubic and a
// sample taken anywhere along it (a tick boundary) sees no jump.
inline void advance_axis(FlightAxis& s, double target_rate, double dt, double max_rate, double max_acc, double max_jerk) {
    if (dt <= 0) return;
    target_rate = std::max(-max_rate, std::min(max_rate, target_rate));
    const double gap = target_rate - s.v;
    double want = std::min(max_acc, std::sqrt(2.0 * max_jerk * std::fabs(gap)));
    if (gap < 0) want = -want;
    const double j = std::max(-max_jerk, std::min(max_jerk, (want - s.a) / dt));
    s.x += s.v * dt + 0.5 * s.a * dt * dt + j * dt * dt * dt / 6.0;
    s.v += s.a * dt + 0.5 * j * dt * dt;
    s.a += j * dt;
}

struct TakeStats {
    int ticks_sampled = 0, ticks_skipped = 0;
    int rule_firings = 0, releases = 0;
    int takeover_tick = -1;
    bool takeover_ok = false, seventh_ok = false;
    // C after the takeover and D: the played person in the game's third person (hold_view)
    int view_mode_restores = 0;   // frames the game was in first person or cinematic and was put back
    int body_turns = 0;           // times the program turned the standing body after its view
    int glide_frames = 0;         // frames the program set the standing body's heading itself (turn "glide")
    int stance_starts = 0;        // times the stance clip was started on the played person standing
    bool logs_ok = true;       // every log line reached its file, and the files closed cleanly
    int cams_read_late = 0;    // ticks whose cameras were read after the fact, not on their boundary
    int cleared = 0;           // the map's objects the spec's clear lines removed before the recorder started
    int cleared_left = -1;     // how many of them were there again as the take ended (-1: not counted)
    std::string last_problem;
};

class Take {
public:
    Take(Runtime& rt, const StageSpec& spec, const Calibration& cal, const Plan& plan, TakeConfig cfg, Hooks hooks);
    ~Take();
    // Where a frame's time went inside the take, milliseconds (the recorder's
    // per-frame timing file): to find what stalls a recording, measured, not guessed
    struct FrameTiming {
        double fly = 0, cams = 0, sample = 0, drive = 0, write = 0, rules = 0, verify = 0, pose = 0, total = 0;
        int tick = -1;   // the tick this frame started, else -1
    };
    const FrameTiming& timing() const { return timing_; }
    Take(const Take&) = delete;
    Take& operator=(const Take&) = delete;

    // Before the recorder: the player in its role, the cast in its tick-0 pose.
    std::string prepare();
    // The recorder came on; tick 0 plays at game time perf0_ms. Opens the logs.
    std::string begin(long long perf0_ms, const std::string& source_dir);
    // Once per frame, after the frame advanced. Returns "" or a fatal error.
    std::string frame(long long game_ms);
    // Stops driving, writes the last tick, gives the player its own body back,
    // removes the cast. Safe to call twice.
    void end();

    const TakeStats& stats() const { return stats_; }
    std::string world_path() const { return world_path_; }
    std::string input_path() const { return input_path_; }
    std::string rules_path() const { return rules_path_; }

private:
    struct Pending {
        int tick = -1;
        long long game_ms = 0;
        std::string wall;   // local system time the tick was posed, YYYY-MM-DD HH:mm:ss.SSS
        int skipped_before = 0;
        bool sampled = false;
        std::vector<EntitySample> actors, props;
        EntitySample player;
        std::vector<MapSample> map;      // StageSpec::tracks
        float wind_speed = 0;            // GET_WIND_SPEED, m/s
        float wind_dir[3] = {0, 0, 0};   // GET_WIND_DIRECTION
        std::vector<int> beats;
        std::vector<bool> released;
        std::string cams;   // the input log's camera part, sampled with the actors
    };
    struct FreeCam {
        int cam = 0;
        FlightAxis px, py, pz, yaw, pitch;   // metres; degrees
        double fov = 50;
        double t_ms = 0;               // game time the state is at
        int next_tick = 0;             // the next tick boundary to record
    };
    // The cameras on one tick boundary, exactly: the free camera as integrated,
    // the game camera and the controlled head interpolated between the frames
    // either side -- samples on the 30 Hz grid, not wherever a frame fell.
    struct CamSample {
        bool free = false;
        double fpos[3] = {}, frot[3] = {}, ffov = 0, fvel[3] = {}, fyaw_rate = 0, fpitch_rate = 0;
        double gpos[3] = {}, grot[3] = {}, gfov = 0;
        std::string cid;               // "" when nobody is played
        double head[3] = {}, cpos[3] = {}, ch = 0;
    };
    struct CamSnap {                   // the same, read on one frame
        bool have = false;
        long long t = 0;
        double gpos[3] = {}, grot[3] = {}, gfov = 0;
        std::string cid;
        double head[3] = {}, cpos[3] = {}, ch = 0;
    };

    int controlled_ped() const;
    std::string controlled_id() const;
    void each_frame(long long game_ms);
    void sample_pending();
    void finish_tick(long long frame_now);
    void write_rule_event(int tick, const std::string& rule, const std::string& when, const std::string& subject,
                          double value, const std::vector<std::pair<std::string, std::string>>& affected, const std::string& say);
    void evaluate_rules(int k);
    void take_over(int k);
    void hold_view();
    void hold_stance(int me);   // spec stance: how the played person stands while the keys rest
    void react(int actor, const std::string& action, int by, int k);
    // A "watch" reaction watches for a while, then the watcher goes back where it stood,
    // turns the way it faced and rejoins the plan at the tick it left (carry_on).
    struct Watcher {
        int actor = -1, until = 0;
        int plan_tick = 0;                         // the plan's tick it was playing when it stopped
        double x = 0, y = 0, z = 0, heading = 0;   // where it stood and faced then
    };
    struct Routine {
        enum Step { Back, Turn, Rejoin };
        Watcher w;
        std::vector<Step> steps;
        size_t at = 0;
        int until = 0;
        bool finished = false;
    };
    void carry_on(const Watcher& w, int k);
    void start_step(Routine& r, int k);
    void advance_routine(Routine& r, int k);
    void fly(long long now_ms, double frame_dt);
    // The program that plays instead of a person when the spec names one
    // (stage_robot.hpp): it sees the frame, then presses keys and moves the mouse
    // through SendInput -- only while the game has the foreground.
    void drive(long long game_ms, double dt);
    void press(const robot::Input& in);
    void release_keys();
    void stop_robot(const std::string& why);
    void record_free(int k);
    CamSnap read_cams(long long now_ms) const;
    void capture_cams(long long now_ms);
    std::string cams_json(int k);
    void accumulate_input();

    Runtime& rt_;
    const StageSpec& spec_;
    const Calibration& cal_;
    const Plan& plan_;
    TakeConfig cfg_;
    Hooks hooks_;
    TakeStats stats_;

    int own_body_ = 0;          // the player's story character
    int seventh_ = 0;           // D: the spawned seventh person
    int takeover_index_ = -1;   // C: actor index
    bool taken_ = false;
    int saved_view_mode_ = -1;  // the game camera's follow-ped view mode before the take, put back after it
    bool turning_ = false;      // the program is turning the standing body (hold_view)
    double turn_to_ = 0;        // toward this world heading
    long long turn_ms_ = 0;     // since this game time
    double body_to_ = std::nan("");   // where the program that plays means the standing body to face (world)
    robot::TurnStyle turn_style_;      // how it turns there (cfg_.turn)
    std::unique_ptr<robot::Flyer> flyer_;     // AB, when a program flies B
    std::unique_ptr<robot::Walker> walker_;   // C/D, when a program plays the person
    std::unique_ptr<robot::RouteWalker> router_;   // C, when the program walks a fixed route
    bool robot_ = false, robot_stopped_ = false;
    long long control_ms_ = -1;               // game time the program took the controls
    robot::Keys held_;
    long long held_since_[6] = {};            // frame each held key went down
    int still_frames_ = 0;                    // frames the played person has stood with the keys at rest (stance)
    FreeCam free_;
    FrameTiming timing_;
    std::map<int, CamSample> cams_;  // per tick boundary, until its tick is written
    CamSnap last_snap_;
    std::set<std::string> fired_;
    // Rules answer what the person did: none before the first key that moves or acts
    // (a taken body starts where the plan left it, maybe right beside somebody), and a
    // "near" rule only once the person has been out of its reach since (an approach).
    bool acted_ = false;
    std::set<std::string> near_armed_;
    std::vector<Watcher> watching_;   // watching now
    std::vector<Routine> routines_;   // on their way back into the plan

    long long perf0_ = 0, last_game_ms_ = 0;
    long long prev_frame_ms_ = -1;   // the frame before's game time: what this frame's reads show
    long long frames_ = 0, posed_frame_ = 0;
    int last_tick_ = -1;
    bool ended_ = false;
    Pending pending_;
    // this tick's input, accumulated over its frames
    std::set<int> keys_;
    double mouse_dx_ = 0, mouse_dy_ = 0;
    int input_frames_ = 0;
    std::vector<std::string> events_;

    std::string world_path_, input_path_, rules_path_;
    std::string scan_dir_;   // the take's source folder: map-scan-begin/end.jsonl (Runtime::scan_objects)
    void write_scan(const std::string& name);
    std::ofstream world_, input_, rules_;
};

// JSON string escaping for the logs (UTF-8 passes through).
std::string json_str(const std::string& s);


}  // namespace diogenes::gta::stage
