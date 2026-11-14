#pragma once

// Stage plan: the actors of a four-worldline take, resolved into what every
// actor does on every 30 Hz game-clock tick. Portable, std-only; the game side
// (stage_runtime) measures the clips and the ground and executes the plan.
//
// Why it is built this way (rehearsal probes T2/A6/v3/v4, issue #958):
//   - an actor's position is a pure function of its synchronized scene's origin
//     and phase, so both are computed here from the tick number, never read
//     back from the engine. Two takes that resolve the same plan put every
//     untouched actor in the same place to the last float bit.
//   - a walk follows an authored path by moving its scene origin every tick:
//     origin = body position on the path - clip root offset at this phase.
//   - a change of clip is a blend or a snap, per stage and per actor. A blend is
//     advanced by the engine on real time, so for a few ticks the pose differs
//     slightly from run to run (the position does not: the mover snaps); the
//     customer accepts that (#958, 2026-09-23). A snap is exact but pops, so the
//     planner chooses WHEN a change happens either way: the phases on both sides
//     are picked so the two poses are as close as the clips allow, and a stop
//     stands exactly where the walk left the body.

#include <cstdint>
#include <functional>
#include <string>
#include <vector>

namespace diogenes::gta::stage {

constexpr int kTickHz = 30;
constexpr int kPoseBones = 6;   // Spine3, Head, L_Hand, R_Hand, L_Foot, R_Foot, root-relative

// ------------------------------------------------------------------ geometry

// GTA heading: 0 is +Y, 90 is -X (matches story_route.hpp and tape.py).
double world_dir_to_heading(double dx, double dy);
double norm_heading(double h);
double heading_delta(double a, double b);   // |a-b| folded into [0,180]

// The stage is a fixed place in the world, owned by the scenario -- never "where
// the player happens to stand": four worldlines must share absolute positions.
struct StageFrame {
    double x = 0, y = 0;        // world position of stage (0,0)
    double z = 0;               // about the ground's height there: ground probes start 5 m above it
    double heading_deg = 0;     // world heading of stage +y
    void to_world(double xs, double ys, double& wx, double& wy) const;
    double heading_to_world(double stage_heading_deg) const;
};

// A walkable path in world coordinates, sampled every 2 cm and indexed by arc
// length, so walking it at a constant speed is a lookup.
struct Path {
    std::vector<double> s, x, y, z;
    bool loop = false;
    double total() const { return s.empty() ? 0.0 : s.back(); }
    // position, ground height and heading at arc length d (wraps if a loop,
    // extrapolates straight along the end tangent past either end otherwise)
    void at(double d, double& ox, double& oy, double& oz, double& heading_deg) const;
    void translate(double dx, double dy);
};

// Straights and arcs in stage coordinates.
struct PathBuilder {
    double x, y, th;   // position and direction (math angle, radians)
    std::vector<double> px, py;
    PathBuilder(double x0, double y0, double stage_heading_deg);
    void line(double len);
    void arc(double r, double dth_deg);   // positive turns left
};
Path make_path(const StageFrame& frame, const PathBuilder& b, bool loop,
               const std::function<double(double, double)>& ground);
void probe_ground(Path& p, const std::function<double(double, double)>& ground);

// ------------------------------------------------------------------ the spec

enum class ClipKind { Loop, Once, Walk };
enum class BeatKind { Idle, Walk, Once, Pair };

struct ClipSpec { std::string id, dict, clip; ClipKind kind = ClipKind::Loop; };
struct PairSpec { std::string id, clip_a, clip_b; double x = 0, y = 0, heading = 0; };   // stage coords
struct LookSpec { int component = 0, drawable = 0, texture = 0; };
struct PropSpec { std::string model; int bone = 0; };
// When an actor's hand props show (propshow <actor> <clip> <from> <to>): only while it plays
// one of these clips between these phases (0..1). An actor with no such line always shows
// them. The phone was attached to the hand all take: put away, it lay on the caller's back,
// then sat in his open hand as he walked (the customer, 2026-09-29).
struct PropShow { std::string clip; double from = 0, to = 1; };
struct PathSpec {
    std::string id; bool loop = false; double x0 = 0, y0 = 0, heading = 0;
    struct Seg { bool arc = false; double a = 0, b = 0; };   // line: a=len; arc: a=r, b=dth
    std::vector<Seg> segs;
};
struct BeatSpec {
    BeatKind kind = BeatKind::Idle;
    std::string clip;          // idle / walk / once
    std::string path;          // walk
    std::string pair;          // pair
    char pair_role = 'a';
    double seconds = 0;        // idle: nominal length (the planner moves it by under one cycle)
    bool has_at = false; double at_x = 0, at_y = 0, at_heading = 0;   // stage coords; first beat only
};
// A causal rule (M5): what the controlled person does to whom, and who reacts
// how. Evaluated only while a person is under a player's control; the actors it
// names leave the plan and are handed to the game's own AI. Nobody else reacts:
// actors are puppets, so a reaction nobody wrote does not happen.
//   rule <id> <near|touch|aim|damage> <actor> [<metres>] then <actor>:<action>[,...] say <Chinese text>
//   action: flee | handsup | cower | fight | watch (5 s, then back to its own thing:
//   Take::carry_on)
struct RuleSpec {
    std::string id, when, subject;   // when: near / touch / aim / damage
    double metres = 0;                // near only
    std::vector<std::pair<std::string, std::string>> then;   // (actor, action)
    std::string say;                  // the causal record's Chinese semantics, to the end of the line
};

struct ActorSpec {
    std::string id, model;
    double blend_in = -1;      // how this actor changes clip: -1 the stage's way, 0 snap, >0 blend-in speed
    std::vector<LookSpec> looks;
    std::vector<PropSpec> props;
    std::vector<PropShow> prop_shows;   // empty: the props always show
    std::vector<BeatSpec> beats;
    int repeat_from = -1;      // after the last beat, continue from this beat index
};
// Who plays a take instead of a person (2026-09-24, stage_robot.hpp):
//   robot <AB|C|D> <mode> <seed>
// AB: fly (worldline B's free camera); C and D: changed (walks up to the cast)
// or unchanged (keeps away from it).
struct RobotSpec {
    std::string take, mode;
    std::uint32_t seed = 0;
};
// A still piece of the scene (the customer, 2026-09-27: the scene is too empty -- add
// cars and things that belong there, not only people): put on the ground before the
// recorder starts, frozen, the same in every take. The first model the game has is
// used; a vehicle is parked (engine off, locked).
//   set <id> <model>[|<model>...] <x> <y> <heading>      stage coords
struct SetSpec { std::string id; std::vector<std::string> models; double x = 0, y = 0, heading = 0; };
// A thing of the map's own the take logs every tick (the customer's delivery doc v1.1: every moving thing the
// cameras see goes into multicam_sync -- the pier's ferris wheel, the beach's flags): the nearest object of the model
// within radius metres (5 if not given) of stage (x, y), found as the cast is spawned; <model> a model name or
// 0x<hash> (what the take's map-scan files list).
//   track <id> <model> <x> <y> [<radius>]                stage coords
struct TrackSpec { std::string id, model; double x = 0, y = 0, radius = 5; };
// Things of the map's own a take removes before the recorder starts and keeps out to its end: every object of the
// model within radius metres of stage (x, y) is deleted and the model hidden there till the cast is despawned. The
// floats, buoys and jet-ski ramps in the sea move with the waves, never the same way in two takes, so the takes of a
// group never agreed on them (multicam_sync, 1.6-1.8 m; the user, 2026-10-18: take them out). <model> boats: every
// boat and jet ski (vehicle class 14) there instead -- the jet skis and the speedboat moored under the pier bob too --
// with the area's vehicle generators off and the models deleted suppressed, so none is put back till the despawn.
//   clear <model|boats> <x> <y> <radius>                 stage coords, radius 1..300 m
struct ClearSpec { std::string model; double x = 0, y = 0, radius = 0; };
// Not every float can be taken out: the map's own (the red buoys, the floating dock under the pier, the jet-ski ramps)
// a replay puts back as it plays and bobs on its own waves, whatever the take did to them -- deleted before the
// recorder (111) or moved and frozen there (112), they were in the export all the same, bobbing (2026-10-18). Only
// what the game spawns of itself (the rope floats, the boats) stays out.
// An actor sitting on a set piece: its seated clip plays where the game itself seats a
// person on that piece (SeatCal).
//   seat <actor> <set> <loop clip>
struct SeatSpec { std::string actor, set, clip; };
// A clip played a little higher or lower than its measured height (lift <clip> <metres>,
// -0.1..0.1; on top of the stage's sink). The measured height puts the ankle where it is
// when the model stands; where the sole then is depends on the shoe and on whose clip it
// is -- a man's photo clip sets a woman's heeled foot flat, toes into the sand -- so the
// last centimetres are set by eye, in a rehearsal (the user, 2026-09-29). Not for a seated
// clip or a pair's (their heights are the seat's and the pair's).
struct LiftSpec { std::string clip; double metres = 0; };
// How a clip's height follows its feet (contact <clip|*> <frame|const|off>, 2026-10-17: the user saw feet go into the
// sand and float out again): frame, the origin moves every phase so the lowest contact point stands on the sand;
// const, one height for the clip, the lowest its contact goes; off, the measured zoff alone (as till then). Unset:
// frame for a clip that stands, const for a walk (a run leaves the ground on purpose).
enum class ContactMode { Default, Frame, Const, Off };
struct ContactSpec { std::string clip; ContactMode mode = ContactMode::Default; };
struct StageSpec {
    StageFrame frame;
    int ticks = 0;
    double blend_in = 0;       // the stage's way to change clip: 0 snap, >= 4 blend-in speed (4 = a quarter second)
    // Metres every scene but a seat is lowered from its measured height (sink <m>): feet
    // a little into the sand rather than a little over it. The game draws the sand a
    // centimetre or two off the ground it stands people on, and a gap reads as floating.
    double sink = 0;
    // The game's own leg IK on the cast (legik <0..3>, SET_PED_LEG_IK_MODE: 0 off, 1 partial, 2 full, 3 full melee):
    // the clips are played as synchronized scenes, one origin height for both feet, so on the beach's uneven sand one
    // foot sank and the other floated (the user, 2026-10-17). -1, unset: the game's default, untouched. Its optional
    // second number goes to TASK_SYNCHRONIZED_SCENE as the IK control flags (0, the default: none).
    int leg_ik = -1, scene_ik_flags = 0;
    // How the played person (C's taken actor once taken, D's seventh) stands when the keys rest (2026-10-18): game,
    // GTA's own idle -- breathing and the weight shifting swayed the head 5-8 cm sideways every second or two, and the
    // first person, which rides the head, with it (beach-demo-113); calm, the game's ambient, gesture and look-at
    // animation off; clip, calm and the dictionary's clip looped standing, blended out as the keys go down (no cast
    // clip: nothing to calibrate, the take loads it itself).
    //   stance game|calm|clip <dict> <clip>
    std::string stance = "game", stance_dict, stance_clip;
    std::vector<ClipSpec> clips;
    std::vector<PairSpec> pairs;
    std::vector<PathSpec> paths;
    std::vector<ActorSpec> actors;
    std::vector<RuleSpec> rules;
    std::vector<RobotSpec> robots;
    std::vector<SetSpec> sets;
    std::vector<TrackSpec> tracks;
    std::vector<ClearSpec> clears;
    std::vector<SeatSpec> seats;
    std::vector<LiftSpec> lifts;
    std::vector<ContactSpec> contacts;   // per clip, "*" for all the rest
    double lift(const std::string& clip) const;   // 0 for a clip with no lift
    const ClipSpec* clip(const std::string& id) const;
    const PairSpec* pair(const std::string& id) const;
    const PathSpec* path(const std::string& id) const;
    const RobotSpec* robot(const std::string& take) const;
    const SetSpec* set(const std::string& id) const;
    const TrackSpec* track(const std::string& id) const;
    const SeatSpec* seat(const std::string& actor) const;
};

// Line format "dm-gta-stage-spec/1", written by gta_capture/stage.py. Returns an
// empty string on success, otherwise what is wrong with which line: anything
// not understood is refused, never skipped.
std::string parse_stage_spec(const std::string& text, StageSpec& out);

// ------------------------------------------------------------------ calibration

// Measured in the game once per group and reused by every take, so all three
// resolve the same plan. Per phase j (tick j of one cycle, sampled at (j+.5)/n).
struct ClipCal {
    std::string id;
    double duration_s = 0;
    int n = 0;                          // ticks per cycle
    double zoff = 0;                    // origin height above the ground that puts the feet on it
    std::vector<double> root_x, root_y; // entity offset from the scene origin, scene heading 0
    std::vector<double> yaw;            // entity heading minus scene heading
    std::vector<double> pose;           // n * kPoseBones * 3, root-relative, entity frame
    double speed = 0;                   // walk clips: m/s along the direction of travel
    // Per phase, metres the lowest contact point -- an ankle against the model's ankle standing on the sand, or a toe
    // against its toe -- is over where it would stand, the origin zoff over the ground: 0 where a foot is planted,
    // over 0 with both feet up, under 0 where a pointed toe goes lower than a standing one (the tourist's heels).
    // Empty in a calibration made before 2026-10-17: no correction. See StageSpec::contacts.
    std::vector<double> contact;
    int pose_at(int j, int b, int c) const { return (j * kPoseBones + b) * 3 + c; }
    double contact_at(int j) const { return contact.empty() ? 0.0 : contact[static_cast<size_t>(j)]; }
    double contact_min() const { double m = 0.0; for (const double v : contact) m = std::min(m, v); return contact.empty() ? 0.0 : m; }
};
// Where the game seats a person on a set piece, as the scene origin of the actor's
// seated clip in the piece's frame: x, y as if the piece faced world heading 0, z above
// its base, heading from its heading. Measured by seating a person there with the
// game's own scenario (Runtime::calibrate). found = false: the game seated nobody on
// it, and the actor plays at its first beat's "at".
struct SeatCal { std::string actor; bool found = false; double x = 0, y = 0, z = 0, heading = 0; };
struct Calibration {
    std::vector<ClipCal> clips;
    std::vector<SeatCal> seats;
    const ClipCal* clip(const std::string& id) const;
    const SeatCal* seat(const std::string& actor) const;
};
// Sum over the pose bones of the distance between two poses, metres.
double pose_distance(const ClipCal& a, int ja, const ClipCal& b, int jb);
// Text form "dm-gta-stage-calibration/1": written by the take that measured it,
// read back by the other takes of the group. Numbers are printed with enough
// digits to round-trip a double exactly, so a plan resolved from the file equals
// the plan resolved from the measurement.
std::string render_calibration(const Calibration& cal);
std::string parse_calibration(const std::string& text, Calibration& out);

// ------------------------------------------------------------------ the plan

struct ResolvedBeat {
    BeatKind kind = BeatKind::Idle;
    std::string clip;          // the clip this actor plays (pair: its own half)
    int pair_index = -1;       // index into StageSpec::pairs
    char pair_role = 0;        // pair: 'a' or 'b'
    int start = 0, ticks = 0;  // [start, start+ticks) in take ticks
    int phase0 = 0;            // cycle phase of the beat's first tick
    int cycle = 0;             // ticks per cycle the phase runs over (a pair: both halves' longest)
    int path = -1;             // walk: index into Plan::paths
    double speed = 0;          // walk: m/s along the path
    double ox = 0, oy = 0, oz = 0, heading = 0;   // idle/once/pair: scene origin (z incl. zoff)
    double ground = 0;         // ground height under the scene
    double entry_pop = 0;      // pose distance at the change into this beat (m, summed over bones)
    double blend_in = 0;       // how this beat is entered: 0 snap, >0 blend-in speed
    bool seated = false;       // on a set piece's seat, where the game put a person: its height kept as measured
    std::string partner_clip;  // pair: the other half's clip (the one scene moves by the middle of the two's contact)
};
struct ActorPlan { std::string id; std::vector<ResolvedBeat> beats; };
struct SetPlacement { std::string id; double x = 0, y = 0, z = 0, heading = 0; };   // world; z: the ground under it
struct Plan {
    StageFrame frame;
    int ticks = 0;
    double sink = 0;                  // StageSpec::sink
    int leg_ik = -1, scene_ik_flags = 0;   // StageSpec::leg_ik, scene_ik_flags
    std::vector<LiftSpec> lifts;      // StageSpec::lifts
    std::vector<ContactSpec> contacts;   // StageSpec::contacts
    ContactMode contact_mode(const std::string& clip, bool walk) const;
    // metres to lower the origin from zoff at phase j of a clip, by its contact mode
    double contact_correction(const ClipCal& cc, int j, bool walk) const;
    double lift(const std::string& clip) const;
    std::vector<Path> paths;
    std::vector<ActorPlan> actors;
    std::vector<SetPlacement> sets;   // in StageSpec::sets order
    std::vector<std::string> notes;   // what the planner had to adjust, for the log
};

// What one actor does on one tick.
struct TickOp {
    int beat = -1;             // index into ActorPlan::beats
    bool enter = false;        // first tick of the beat: into a new scene (snap or blend, see blend_in)
    double blend_in = 0;
    bool shared = false;       // pair beat: one scene for both, built by the first of the two
    double ox = 0, oy = 0, oz = 0, heading = 0;   // scene origin this tick
    float phase = 0;
    double want_x = 0, want_y = 0, ground = 0;    // walks: where the body should be
    bool walking = false;
    bool contact = false;      // oz moved off the beat's origin this tick for the feet (StageSpec::contacts)
};

// Resolves every actor's beats into ticks, anchors and phases. Returns an empty
// string on success.
std::string resolve_plan(const StageSpec& spec, const Calibration& cal,
                         const std::function<double(double, double)>& ground, Plan& out);
TickOp tick_op(const Plan& plan, const Calibration& cal, int actor, int tick);

// Game time to tick: k = floor((game_ms - perf0_ms) * 30 / 1000). A capped
// 1/15 s game step spans two ticks, so skipped ticks are expected.
inline int tick_of(long long game_ms, long long perf0_ms) {
    const long long d = game_ms - perf0_ms;
    return d < 0 ? -static_cast<int>((-d * kTickHz + 999) / 1000) : static_cast<int>(d * kTickHz / 1000);
}

// How far game time lies from the middle of tick_of's tick, in ms: -16.7 up to
// +16.7. The recorder's 30 fps cap holds every frame near 0, so each tick gets
// exactly one frame: at 30 frames a second a frame on a tick's edge would land
// in either tick at a millisecond's jitter.
inline double tick_offset_ms(long long game_ms, long long perf0_ms) {
    long long r = ((game_ms - perf0_ms) * kTickHz) % 1000;   // 1000 a tick
    if (r < 0) r += 1000;
    return (static_cast<double>(r) - 500.0) / kTickHz;
}

// The frame cap's next deadline, on the performance counter (ms): a tick after the
// last one, pulled half the way to the middle of the tick (offset: this frame's
// tick_offset_ms). A game more than a frame behind starts over from now rather
// than catch up in a burst of short frames.
inline double next_frame_deadline(double last_deadline_ms, double now_ms, double offset_ms) {
    const double period = 1000.0 / kTickHz;
    const double next = last_deadline_ms + period - 0.5 * offset_ms;
    return next < now_ms - period ? now_ms : next;
}

// A stable text rendering of a plan, hashed into the take's evidence: three
// takes of one group must produce byte-identical renderings.
std::string render_plan(const Plan& plan);

}  // namespace diogenes::gta::stage
