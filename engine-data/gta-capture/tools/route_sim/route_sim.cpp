// Route simulator for take C (and D's stroller): runs the program's walker (stage_robot.hpp) against the
// cast as recorded in AB, with the person moved the way the game moves it, and judges each run as a take.
//
// The game's walking, measured on beach-demo-003 C (2026-09-28, the take that slid into the chatting pair
// and circled) and the earlier takes:
//   start   straight along the way the keys point (W: the camera; W+A / W+D: 45 deg left / right of it) as
//           they went down (+-5 deg) for 1.0-1.2 s; speed 0.5 / 0.9 / 1.1 / 1.14 / 1.04 / 1.29 / 1.8 / 1.9 m/s
//           at 0.1..1.2 s, then 1.72
//   walking the way follows the keys' way, K 3.0-4.0 /s (18.5 deg behind at 60 deg/s)
//   let go  slides 0.52-1.51 m (at walking speed), sideways up to ~0.15 m toward a turn
//   body    turns after the way at up to 180 deg/s; standing, the take turns it where the program means it to
//           face (Input::body_to, stage_take_win.cpp hold_view) in 0.5-1 s whatever the angle (beach-demo-003 D,
//           2026-10-09), as stage_plan_selftest's body_turn
// The rules as in the spec: near_pair (chat_a 1.2 m: chat_a, chat_b watch), block_tourist (tourist 1.0),
// block_jogger (jogger 0.8). A watcher stands 5 s where it was, turns back and goes on 5.0-6.5 s behind.
//
// A run is a good take when: both story rules fire and the jogger's does not, every stop is reached and nobody
// held the walk up, nobody is touched (closest: chat_a 0.8, chat_b 0.9, tourist 0.72, sitter 1.2, jogger 1.5 m),
// and the first-person lane sees people (below).
//
// The first-person lane is judged as subject_bones.py judges the delivered one (rev 630: 80% of the time, 96 s of
// 120): the seven body points of every other person (bones.csv) through the FP_MM (35) mm, 1920x1080 view of the played
// head; "half" is 4+ points in frame, "any" is 1+. Before the takeover the lane rides the head of the actor to be
// taken over. Checked against two recorded takes (tools/route_sim/README.md): the count is 1-2 points optimistic.
// The view looks where the body faces (the customer, 2026-10-16: the first person goes with the head, which in GTA
// goes with the body; the camera is the third person's, the mouse's), level (stage_tape.FP_PITCH, 2026-10-18; at the
// camera's pitch before); FP_VIEW=camera looks where the camera does, as the lane did from 2026-10-07 (to replay those
// takes).
//
// Inputs (directory SP): cast.csv and bones.csv, made from AB's world telemetry by make_tables.py.
//   ROUTE    the route (required; the group config's takeover.route)   SEED0/SEEDS  seeds to run (20260928 / 1)
//   FP_MM    the first-person lens, mm (35, stage_tape.FP_FOCAL_MM; 18 for the takes before 2026-10-16)
//   FP_ALL / FP_POST  what a take needs of the first-person lane, % of the whole 120 s / of the time after the
//            takeover (80 / 90; 0 turns the check off)      TAKEOVER  the takeover second (11)
//   VERBOSE  stops and rules per run      PATHLOG  per-frame path to stderr (lanecheck.py reads it)
//   FPMAP    the first-person lane's visibility by 5 s, over the seeds      FPDUMP  per-frame view count to stderr
//   REPLAY   a recorded take (replay_fp.py) instead of a sim; REPLAY_DUMP prints the per-frame count
//   FP_VIEW  "camera": the first person looks the game camera's way (the takes of 2026-10-07 to -16), not the body's
//   KIND=D   D's take (the seventh person from tick 0), WALKER=1 with D's stroller; WSEED its seed
//   PAUSE    D's stroller: "min max" seconds a stop lasts (WalkRoom pause_min / pause_max); STEPS_IN its walks in
//            before one back out
//   TURN     how the standing body turns (stage_robot.hpp TurnStyle; the group config's "turn"): "game" (one go,
//            0.5-1 s whatever the angle, as 003 recorded), "steps" (the game's turn 30 degrees at a time) or
//            "glide <deg/s> <deg/s^2>" (the take sets the heading each frame: the body is where the program says)
// Every run also reports the first-person view's turn (smoothed as stage_tape smooths the lane): seconds over
// 90 deg/s and over 60, its p95, the fastest, acceleration p95 and jerk p95 -- a run over the requirement's limits
// (90 / never over 90 / 150 / 600) is a bad run, TURN_LIMITS=0 leaves them out -- and the standing turns' angles; and the keys each
// walk went on (W, A, S, D and the pairs: the customer, 2026-10-16, not W alone).
#include "stage_robot.hpp"
#include <algorithm>
#include <array>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <random>
#include <sstream>
#include <string>
#include <vector>
namespace rb = diogenes::gta::stage::robot;
constexpr double kPi = 3.14159265358979323846;

static double start_speed(double t) {
    static const double T[][2] = {{0.0, 0.0}, {0.1, 0.5}, {0.2, 0.9}, {0.3, 1.1}, {0.4, 1.14}, {0.6, 1.04}, {0.8, 1.29}, {1.0, 1.8}, {1.2, 1.9}, {1.5, 1.72}};
    if (t >= 1.5) return 1.72;
    for (int i = 0; i + 1 < 10; ++i)
        if (t <= T[i + 1][0]) return T[i][1] + (T[i + 1][1] - T[i][1]) * (t - T[i][0]) / (T[i + 1][0] - T[i][0]);
    return 1.72;
}


// ------------------------------------------------------------------ the first-person lane's view
// The first-person lens on a 36 mm film back, 1920 px wide: FP_MM, 24 mm by default (stage_tape.FP_FOCAL_MM since
// 2026-10-16, the customer: the first person was too wide at 18 mm; FP_MM=18 replays the takes before).
static const double kFx = 1920.0 * (std::getenv("FP_MM") ? std::atof(std::getenv("FP_MM")) : 35.0) / 36.0;
constexpr double kEyeAhead = 0.20, kHeadZ = 1.62, kMaxPitch = 35.0;   // stage_tape.py
constexpr int kEaseTicks = 20;
// The first-person lane turns with the body, no lag (stage_tape, the customer 2026-10-16). FP_RATE / FP_ACCEL bring it
// round slower, eased (a try of 2026-10-16 the customer turned down: 60, 120): to compare
static double g_fp_rate = 0.0, g_fp_accel = 120.0;
// A 0.43 s tap on two keys (W+D, S+A ...) often walks instead, 0.85-1.10 m, the body turned all the way to the keys'
// way; on one key it never did (win-pc 2026-10-17, beach-demo-101's C and D, 102's C, wind-test-1017's probe: two keys
// 16 of 41, one key 0 of 16): TAP_WALK_P, the two-key taps' chance, 0.4 by default. The controller taps on one key
// (stage_robot key_for) unless the route names two.
static double g_tap_walk_p = 0.4;
// The game camera swings round behind a person who walks, the mouse still (win-pc arc-test-1016b, 2026-10-16: none
// while standing; walking, toward the back at 16 deg/s with the body 30-60 degrees off it, 22-23 at 60-120, 29 at
// 120-150, 14 -- either way -- beyond), deg/s; the walkers' mouse steers against it.
static double follow_turn(double off, double v) {
    off = rb::wrap(off);
    const double a = std::fabs(off);
    if (v < 0.5 || a < 1e-9) return 0.0;
    const double rate = a < 30.0 ? 0.5 * a : a <= 150.0 ? std::min(30.0, 13.0 + 0.12 * a) : 14.0;
    return std::copysign(rate, off);
}
// one frame of stage_tape._swing: x follows target no faster than max_rate, eased at max_accel, braking in time
static void swing_step(double& x, double& v, double target, double dt) {
    const double gap = rb::wrap(target - x);
    const double want = std::copysign(std::min(g_fp_rate, std::sqrt(2.0 * g_fp_accel * std::fabs(gap))), gap);
    v += std::clamp(want - v, -g_fp_accel * dt, g_fp_accel * dt);
    x = rb::wrap(x + v * dt);
}
static double ease(double t) { t = std::clamp(t, 0.0, 1.0); return t * t * t * (10.0 + t * (-15.0 + 6.0 * t)); }

using BoneRow = std::array<double, 126>;   // 5 people x 7 bones x (dx dy z), bones.csv; then D's pacer (bones_pacer.csv)

static bool g_fp_camera = false;   // FP_VIEW=camera

// How many body points of the person seen best are inside the frame (0..7).
// (x y) the played body, `body` its heading, cam/pitch the game camera, `weight` the takeover's ease-in.
// The view looks the way stage_tape.first_person_view does, level (FP_VIEW=camera: at the camera's pitch, as till
// 2026-10-18), the eye kEyeAhead out along it.
static int seen_points(double x, double y, double body, double cam, double pitch, double weight, const double* px, const double* py,
                       const BoneRow& B, int np, double* eye_out = nullptr) {
    const double vp = g_fp_camera ? kMaxPitch * std::tanh(pitch / kMaxPitch) * weight : 0.0;
    const double vy = rb::wrap(body + rb::wrap(cam - body) * weight);
    const double pr = vp * kPi / 180.0, yr = vy * kPi / 180.0;
    const double eye[3] = {x - std::sin(yr) * std::cos(pr) * kEyeAhead, y + std::cos(yr) * std::cos(pr) * kEyeAhead, kHeadZ + std::sin(pr) * kEyeAhead};
    if (eye_out) for (int i = 0; i < 3; ++i) eye_out[i] = eye[i];
    const double f[3] = {-std::sin(yr) * std::cos(pr), std::cos(yr) * std::cos(pr), std::sin(pr)};
    const double r[3] = {std::cos(yr), std::sin(yr), 0.0};
    const double u[3] = {r[1] * f[2] - r[2] * f[1], r[2] * f[0] - r[0] * f[2], r[0] * f[1] - r[1] * f[0]};
    int best = 0;
    for (int i = 0; i < np; ++i) {
        int inside = 0;
        for (int b = 0; b < 7; ++b) {
            const double d[3] = {px[i] + B[(i * 7 + b) * 3] - eye[0], py[i] + B[(i * 7 + b) * 3 + 1] - eye[1], B[(i * 7 + b) * 3 + 2] - eye[2]};
            const double z = d[0] * f[0] + d[1] * f[1] + d[2] * f[2];
            if (z < 0.3) continue;
            const double uu = 960.0 + kFx * (d[0] * r[0] + d[1] * r[1] + d[2] * r[2]) / z, vv = 540.0 - kFx * (d[0] * u[0] + d[1] * u[1] + d[2] * u[2]) / z;
            inside += uu >= 0 && uu <= 1920 && vv >= 0 && vv <= 1080;
        }
        best = std::max(best, inside);
    }
    return best;
}

// The take's turn of a standing body, as stage_plan_selftest models it: a moment after the ask, the faster
// the more is left.
constexpr double kBodyTurnMin = 40.0, kBodyTurnGain = 1.5, kBodyTurnMax = 220.0, kBodyTurnDelay = 0.2;
static double body_turn(double off, double dt) {
    const double rate = std::min(kBodyTurnMax, kBodyTurnMin + kBodyTurnGain * std::fabs(off));
    return std::clamp(off, -rate * dt, rate * dt);
}
static rb::TurnStyle g_turn;   // TURN
// the eight ways keys walk, for the count of what each walk went on
static const char* const kKeyNames[8] = {"W", "W+A", "A", "S+A", "S", "S+D", "D", "W+D"};
static int key_index(const rb::Keys& k) { return (static_cast<int>(std::lround(rb::key_way(k) / 45.0)) + 8) % 8; }
static const char* key_name(const rb::Keys& k) { return kKeyNames[key_index(k)]; }

struct Foot {
    double x = 0, y = 0, body = 0, dir = 0, v = 0;
    bool walking = false, fresh = false, sliding = false;
    double walk_t = 0, lock_dir = 0, dir_rate = 0, stretch = 1.0, turn_wait = 0;
    double slide_v0 = 0, slide_t = 0, slide_T = 1, slide_dir = 0, slide_len = 1, drift = 0;
    double lock_s = 1.1, K = 3.5;
    // a tap -- the keys let go within kTapCommitS of a start from standing -- turns the body on the spot to where the
    // keys pointed, at the game's pace, with no slide (win-pc 2026-10-15: 0.43 s taps 30-90 degrees off, the body round
    // within a degree in ~1.4 s, 44/70/100/165 deg/s at the fastest, 0.24-0.33 m moved, 0.26 mostly; held 0.5 s it
    // walked a metre twice and tapped twice; held 0.53 s and longer it walked)
    static constexpr double kTapCommitS = 0.52, kStepMaxS = 0.65;
    bool tapping = false, two_keys = false;   // and whether the press went down on two keys
    double tap_to = 0, press_x = 0, press_y = 0;   // and where the press began: a tap steps ~0.26 m along its new way
    std::mt19937* rng = nullptr;
    double uni(double a, double b) { return std::uniform_real_distribution<double>(a, b)(*rng); }
    double gauss(double s) { return std::normal_distribution<double>(0.0, s)(*rng); }
    void step(double dt, const rb::Keys& k, double cam, double body_to) {
        const bool w = k.walks();
        bool set = false;   // the take set the heading this frame
        if (w) cam += rb::key_way(k);   // the third person walks the keys' way off the camera
        const double before = dir;
        if (w) {
            tapping = false;
            if (!walking) {
                walking = true; walk_t = 0; sliding = false;
                two_keys = (k.w || k.s) && (k.a || k.d);
                fresh = v < 0.3;
                press_x = x; press_y = y;
                if (fresh) {
                    // measured (207 starts): turned 75 degrees or more, the start swings 2-7 degrees further,
                    // away from the side the body faced; less than that, a few degrees either way
                    const double off = rb::wrap(body - cam);
                    lock_dir = cam + (std::fabs(off) >= 75.0 ? (off < 0 ? 1.0 : -1.0) * uni(2.0, 7.0) : uni(-3.0, 3.0));
                    dir = lock_dir;
                    stretch = 1.0 + 0.3 * std::clamp((std::fabs(rb::wrap(cam - body)) - 90.0) / 90.0, 0.0, 1.0);   // a turn round starts slower
                }
            }
            walk_t += dt;
            v = fresh ? start_speed(walk_t / stretch) : std::min(1.72, v + 2.0 * dt);
            if (fresh && walk_t < lock_s * stretch) dir = lock_dir;
            else dir += rb::wrap(cam - dir) * (1.0 - std::exp(-K * dt));
        } else {
            const bool tap_walks = walking && fresh && walk_t < kTapCommitS && two_keys && uni(0.0, 1.0) < g_tap_walk_p;
            if (walking && fresh && walk_t < kTapCommitS && !tap_walks) {   // a tap: the body comes round, no slide
                walking = false; sliding = false; v = 0.0;
                tapping = true; tap_to = lock_dir;
                const double r = lock_dir * kPi / 180.0;
                const double moved = uni(0.23, 0.33);
                x = press_x - std::sin(r) * moved; y = press_y + std::cos(r) * moved;
            } else if (walking) {
                walking = false;
                sliding = v > 0.05;
                slide_v0 = v; slide_t = 0; slide_dir = dir;
                static const double kSlides[] = {0.52, 0.54, 0.77, 0.80, 0.80, 0.81, 0.84, 0.97, 1.01, 1.03, 1.08, 1.08, 1.25, 1.51};
                slide_len = std::max(0.3, kSlides[(*rng)() % 14] + gauss(0.03)) * std::min(1.0, v / 1.72);
                // a short walk from standing (0.5-0.65 s) stops one of two ways, by where the feet are as the keys come
                // up: at once, 0.64-0.78 m on, or a stride later, 1.09-1.46 m on (win-pc 2026-10-15, ten starts)
                if (fresh && walk_t <= kStepMaxS) slide_len = (*rng)() % 2 ? uni(0.62, 0.80) : uni(1.05, 1.45);
                if (tap_walks) slide_len = uni(0.75, 0.85);   // a tap that walked: 1.05-1.09 m in all
                slide_T = 2.0 * slide_len / std::max(v, 0.05);
                // sideways: toward the turn it was in (+0.13 m at 60 deg/s, measured), and a little either way
                drift = std::clamp(0.0022 * dir_rate + gauss(0.03), -0.25, 0.25);
            }
            if (sliding) {
                slide_t += dt;
                v = std::max(0.0, slide_v0 * (1.0 - slide_t / slide_T));
                if (v <= 0.0) sliding = false;
            } else {
                v = 0.0;
            }
            if (tapping) {   // the game turns the tapped body round, faster the further (1.4 deg/s a degree, 180 at most)
                const double off = rb::wrap(tap_to - body);
                const double rate = std::min(180.0, 1.4 * std::fabs(off) + 10.0);
                body = rb::wrap(body + std::clamp(off, -rate * dt, rate * dt));
                dir = body;
                if (std::fabs(off) < 0.5) tapping = false;
            }
            const double off = sliding || std::isnan(body_to) ? 0.0 : rb::wrap(body_to - body);
            if (g_turn.mode == rb::TurnMode::Glide) {   // the take sets the heading where the program says, every frame
                // no key down, sliding too (stage_take hold_view): set only once the slide was over, the body
                // jumped to where the glide had got to meanwhile (300-1500 deg/s in D's first person, 2026-10-16)
                if (!std::isnan(body_to)) { body = rb::wrap(body_to); set = true; }
            } else {
                turn_wait = std::fabs(off) > 4.0 ? turn_wait + dt : 0.0;
                if (turn_wait > kBodyTurnDelay) body = rb::wrap(body + body_turn(off, dt));
            }
        }
        const double r = dir * kPi / 180.0;
        x += -std::sin(r) * v * dt;
        y += std::cos(r) * v * dt;
        if (sliding && slide_len > 1e-6) {   // left of the way is (-cos, -sin)
            const double share = drift * v * dt / slide_len;
            const double sr = slide_dir * kPi / 180.0;
            x += -std::cos(sr) * share;
            y += -std::sin(sr) * share;
        }
        if (dt > 0) dir_rate = rb::wrap(dir - before) / dt;
        if (v > 0.1 && !set) {
            const double off = rb::wrap(dir - body);
            if (fresh && walk_t < lock_s * stretch) {   // a start turns the body round as a tap does: 35 degrees at ~60 deg/s
                const double rate = std::min(180.0, 1.4 * std::fabs(off) + 10.0);   // (win-pc 2026-10-16, C4 and D6: 73 at most)
                body += std::clamp(off, -rate * dt, rate * dt);
            } else {
                body += std::clamp(off * (1.0 - std::exp(-6.0 * dt)), -180.0 * dt, 180.0 * dt);
            }
        }
        body = rb::wrap(body);
    }
};

int main() {
    const char* sp = std::getenv("SP");
    std::ifstream f(std::string(sp ? sp : ".") + "/cast.csv");
    std::vector<std::array<double, 13>> cast;
    for (std::string line; std::getline(f, line);) {
        std::istringstream s(line); std::array<double, 13> r{};
        for (double& x : r) s >> x;
        cast.push_back(r);
    }
    const int N = static_cast<int>(cast.size());
    std::vector<BoneRow> bones;
    {
        std::ifstream bf(std::string(sp ? sp : ".") + "/bones.csv");
        for (std::string line; std::getline(bf, line);) {
            std::istringstream bs(line); BoneRow r{};
            for (double& x : r) bs >> x;
            bones.push_back(r);
        }
        if (bones.size() < static_cast<size_t>(N)) { std::printf("bones.csv: %zu rows, cast.csv has %d (tools/make_bones.py)\n", bones.size(), N); return 2; }
    }
    const char* ids[6] = {"chat_a", "chat_b", "tourist", "jogger", "sitter", "pacer"};
    const bool D = std::getenv("KIND") && std::string(std::getenv("KIND")) == "D";   // the seventh person, from tick 0: nobody may react
    if (std::getenv("FP_RATE")) g_fp_rate = std::atof(std::getenv("FP_RATE"));
    if (std::getenv("FP_ACCEL")) g_fp_accel = std::atof(std::getenv("FP_ACCEL"));
    if (std::getenv("TAP_WALK_P")) g_tap_walk_p = std::atof(std::getenv("TAP_WALK_P"));
    // the takeover second (C; TAKEOVER, 11 by default): before it the lane rides the head of the actor to be taken
    // over, looking where it faces -- the same in every run (it was 11 s whatever TAKEOVER said till 2026-10-15)
    const int kTakeoverTick = static_cast<int>(std::lround((std::getenv("TAKEOVER") ? std::atof(std::getenv("TAKEOVER")) : 11.0) * 30));
    // the pacer's bones (bones_pacer.csv, make_tables.py): in D he is one of the people the seventh sees
    bool pacer_seen = false;
    if (D) {
        std::ifstream pf(std::string(sp ? sp : ".") + "/bones_pacer.csv");
        size_t k = 0;
        for (std::string line; std::getline(pf, line) && k < bones.size(); ++k) {
            std::istringstream ps(line);
            for (int j = 0; j < 21; ++j) ps >> bones[k][105 + j];
        }
        pacer_seen = k >= static_cast<size_t>(N);
    }
    const int seen_np = D && pacer_seen ? 6 : 5;
    const bool tourist_only = !D && std::getenv("C_TOURIST_ONLY") != nullptr;
    const int NP = D ? 6 : 5;   // in C the pacer is the one played
    const double fp_need_all = std::getenv("FP_ALL") ? std::atof(std::getenv("FP_ALL")) : 80.0, fp_need_post = std::getenv("FP_POST") ? std::atof(std::getenv("FP_POST")) : 90.0;
    // before the takeover (C): the lane rides the head of the actor to be taken over, looking where it faces; the same in every run
    double pre_half = 0, pre_any = 0, pre_w = 0;
    double binsum[24] = {}, pre_binhalf[24] = {};
    if (!D) {
        const int pre_ticks = std::min(kTakeoverTick, N);
        for (int k = 0; k < pre_ticks; ++k) {
            double px[5], py[5];
            for (int i = 0; i < 5; ++i) { px[i] = cast[k][2 * i]; py[i] = cast[k][2 * i + 1]; }
            const int n = seen_points(cast[k][10], cast[k][11], rb::wrap(cast[k][12]), rb::wrap(cast[k][12]), 0.0, 1.0, px, py, bones[k], 5);
            pre_w += 1.0 / 30; pre_half += (n >= 4) / 30.0; pre_any += (n >= 1) / 30.0;
            pre_binhalf[k / 150] += (n >= 4) / 30.0;
        }
    }
    g_fp_camera = std::getenv("FP_VIEW") && std::string(std::getenv("FP_VIEW")) == "camera";
    if (const char* rp = std::getenv("REPLAY")) {   // a recorded take: t x y body cam pitch, then the five people's x y (tools/replay_fp.py)
        std::ifstream rf(rp);
        double half = pre_half, any = pre_any, all = pre_w, post_half = 0, post_any = 0, post_w = 0, gap = 0, gap_start = 0;
        std::vector<std::pair<double, double>> gaps;   // seconds long, from second
        for (std::string line; std::getline(rf, line);) {
            std::istringstream ls(line); double v[16];
            for (double& x : v) ls >> x;
            const int kt = static_cast<int>(std::lround(v[0] * 30));
            if (kt < kTakeoverTick) continue;   // the recording's own pre-takeover ticks are the actor's, not the played body's
            double px[5], py[5];
            for (int i = 0; i < 5; ++i) { px[i] = v[6 + 2 * i]; py[i] = v[7 + 2 * i]; }
            const int n = seen_points(v[1], v[2], v[3], g_fp_camera ? v[4] : v[3], v[5], ease((kt - kTakeoverTick) / double(kEaseTicks)), px, py, bones[std::min(kt, N - 1)], 5);
            if (std::getenv("REPLAY_DUMP")) std::printf("F %d %d\n", kt, n);
            all += 1.0 / 30; post_w += 1.0 / 30; half += (n >= 4) / 30.0; any += (n >= 1) / 30.0; post_half += (n >= 4) / 30.0; post_any += (n >= 1) / 30.0;
            if (n >= 4) { if (gap > 0) gaps.push_back({gap, gap_start}); gap = 0; } else { if (gap == 0) gap_start = v[0]; gap += 1.0 / 30; }
        }
        if (gap > 0) gaps.push_back({gap, gap_start});
        std::sort(gaps.rbegin(), gaps.rend());
        std::printf("replay: before the takeover %.1f s of %.1f s half a person\n", pre_half, pre_w);
        std::printf("replay: first person over the whole take %.1f%% half a person (4+ points) / %.1f%% any (1+); after the takeover %.1f%% / %.1f%%\n",
                    100 * half / all, 100 * any / all, 100 * post_half / post_w, 100 * post_any / post_w);
        std::printf("   longest stretches without half a person (s long, from s):");
        for (size_t i = 0; i < std::min<size_t>(8, gaps.size()); ++i) std::printf(" (%.1f, %.1f)", gaps[i].first, gaps[i].second);
        std::printf("\n");
        return 0;
    }
    if (const char* tt = std::getenv("TURN")) {
        const std::string e = rb::parse_turn(tt, g_turn);
        if (!e.empty()) { std::printf("%s\n", e.c_str()); return 2; }
    }
    const char* rt = std::getenv("ROUTE");
    if (!rt) { std::printf("ROUTE?\n"); return 2; }
    std::vector<rb::RouteStep> steps;
    { const std::string e = rb::parse_route(rt, steps); if (!e.empty()) { std::printf("route: %s\n", e.c_str()); return 1; } }
    size_t stops = 0;   // way points are walked round, not arrived at
    for (const auto& st : steps) stops += st.stand_s > 0.0;
    const double t0 = D ? 0.0 : std::getenv("TAKEOVER") ? std::atof(std::getenv("TAKEOVER")) : 11.0;
    const unsigned seed0 = std::getenv("SEED0") ? std::stoul(std::getenv("SEED0")) : 20260928u;
    const int seeds = std::getenv("SEEDS") ? std::atoi(std::getenv("SEEDS")) : 1;
    const bool verbose = std::getenv("VERBOSE") != nullptr, pathlog = std::getenv("PATHLOG") != nullptr;
    const bool turn_limits = !(std::getenv("TURN_LIMITS") && std::string(std::getenv("TURN_LIMITS")) == "0");
    const double stand_max_s = std::getenv("STAND_MAX_S") ? std::atof(std::getenv("STAND_MAX_S")) : 0.0;
    const int min_walks = std::getenv("MIN_WALKS") ? std::atoi(std::getenv("MIN_WALKS")) : 0;
    // turned only while walking (the user, 2026-10-17: a turn on the spot reads as the feet sliding): START_MAX, the most
    // the body may be off a walk's way as its keys go down (the game turns it on the spot by that much); TAPS_MAX, the taps
    // a run may make (-1: either unchecked)
    const double start_max = std::getenv("START_MAX") ? std::atof(std::getenv("START_MAX")) : -1.0;
    const int taps_max = std::getenv("TAPS_MAX") ? std::atoi(std::getenv("TAPS_MAX")) : -1;
    int bad_runs = 0;
    double worst[6] = {9, 9, 9, 9, 9, 9};
    double over90_sum = 0, over90_max = 0, over60_sum = 0, p95_max = 0, acc95_max = 0, rate_max = 0, turn_max = 0, jerk95_max = 0;
    long key_total[8] = {};   // walks on each of the eight ways, over all runs
    int walks_min = 1 << 30; double walked_min = 1e9;
    int runs_w_only = 0;      // runs that walked on W alone
    int turns_big_max = 0, turns_sum = 0;
    double fp_all_sum = 0, fp_post_sum = 0, fp_all_min = 1e9, fp_post_min = 1e9, gap_worst = 0, stand_worst = 0, press_worst = 99, behind_min = 1e9, behind_sum = 0;
    for (int run = 0; run < seeds; ++run) {
        const unsigned seed = seed0 + run;
        std::mt19937 rng(seed);
        Foot foot; foot.rng = &rng;
        foot.lock_s = foot.uni(1.0, 1.2); foot.K = foot.uni(3.0, 4.0);
        const int k0 = static_cast<int>(t0 * 30);
        foot.x = cast[k0][10]; foot.y = cast[k0][11]; foot.body = foot.dir = rb::wrap(cast[k0][12]);
        if (D) {   // the group's seventh person: D_START=x,y,heading (stage), else where 003 put him
            foot.x = 0.0; foot.y = -6.0; foot.body = foot.dir = 0.0;
            if (const char* ds = std::getenv("D_START")) { std::sscanf(ds, "%lf,%lf,%lf", &foot.x, &foot.y, &foot.body); foot.dir = foot.body; }
        }
        double fp_yaw = foot.body, fp_rate = 0.0;   // the first person's yaw, coming round after the body (swing_step)
        double cam = foot.body, pitch = 0;   // the takeover hands over a level camera at the actor's own heading
        rb::WalkRoom room;
        if (const char* ps = std::getenv("PAUSE")) std::sscanf(ps, "%lf %lf", &room.pause_min, &room.pause_max);
        if (const char* si = std::getenv("STEPS_IN")) room.steps_in = std::atoi(si);
        rb::RouteWalker walker(steps, t0, rb::WalkRoom{}, g_turn);
        const bool use_walker = std::getenv("WALKER") != nullptr;   // D as recorded in 002: the unchanged stroller
        rb::Walker stroller(std::getenv("WSEED") ? std::stoul(std::getenv("WSEED")) : 20260926u, false, 121.0 - t0,
                            {{"chat_a", 1.2}, {"tourist", 1.0}, {"jogger", 0.8}}, room, g_turn);
        int fired[6] = {-1, -1, -1, -1, -1, -1};
        double delay[6] = {0, 0, 0, 0, 0, 0};
        // a watcher walking when its rule fires stops kStopTicks later (beach-demo-003 turn-test-route-C1, 2026-10-15:
        // the tourist at 1.8 m/s stood 0.2-0.3 m on from where it was), then stands, then goes on that much behind
        constexpr int kStopTicks = 4;
        auto at = [&](int i, int k, double& x, double& y, int* frame = nullptr) {
            int kk = k;
            if (fired[i] >= 0 && k >= fired[i] + kStopTicks) {
                const int d = static_cast<int>(delay[i] * 30);
                kk = k < fired[i] + kStopTicks + d ? fired[i] + kStopTicks : k - d;
            }
            kk = std::clamp(kk, 0, N - 1);
            if (frame) *frame = kk;
            x = cast[kk][i < 5 ? 2 * i : 10]; y = cast[kk][i < 5 ? 2 * i + 1 : 11];
        };
        // D also evaluates bump_pacer: derive storage and iteration from the
        // same rule table so its fourth state cannot overrun C's three states.
        const struct { int subject; double metres; int then[2]; const char* id; } rules[] = {
            {0, 1.2, {0, 1}, "near_pair"}, {2, 1.0, {2, -1}, "block_tourist"}, {3, 0.8, {3, -1}, "block_jogger"}, {5, 0.6, {5, -1}, "bump_pacer"}};
        constexpr size_t rule_count = sizeof rules / sizeof rules[0];
        bool armed[rule_count] = {}, acted = false, was_w = false;
        int mdx = 0, mdy = 0, frames = 0, behind = 0, walking = 0;
        double fp_w = 0, fp_half = 0, fp_any = 0, gap = 0, gap_max = 0, gap_at = 0, stand = 0, stand_max = 0, stand_at = 0, press_at = -1, press_min = 99;
        int presses = 0, key_uses[8] = {}, walks_n = 0;   // walks: presses held kWalkHeldS and more (not taps)
        constexpr double kWalkHeldS = 0.53;
        double walked_m = 0;                           // ... and how far the person went, slides and all
        double run_bin[24] = {};
        rb::Keys held;
        double body_to = std::nan("");
        double t = 0, close[6] = {9, 9, 9, 9, 9, 9}, close_t[6] = {0, 0, 0, 0, 0, 0};
        std::vector<std::string> events;
        std::vector<std::pair<double, double>> views;   // (dt, the game camera's yaw) each frame of the take after the takeover
        int turns = 0, turns_big = 0;
        double turn_from = std::nan(""), turn_big = 0;   // a standing turn: where the body faced as it began
        while (t0 + t < 120.9) {
            const double dt = rng() % 50 == 0 ? 1.0 / 15 : 1.0 / 30;
            cam = rb::wrap(cam - 0.08 * mdx);
            pitch = std::clamp(pitch - 0.08 * mdy, -70.0, 45.0);
            cam = rb::wrap(cam + follow_turn(foot.body - cam, foot.v) * dt);   // the game's own swing behind a walker
            const double body_before = foot.body;
            foot.step(dt, held, cam, body_to);
            t += dt;
            if (g_fp_rate > 0) swing_step(fp_yaw, fp_rate, foot.body, dt);   // the first person comes round after the body
            else fp_yaw = foot.body;
            if (t0 + t <= 120.0) views.push_back({dt, g_fp_camera ? cam : fp_yaw});   // the first person's yaw
            {   // standing turns: from the body's way as one begins to where it stops turning
                const bool turning = !(held.w || held.a || held.d) && foot.v < 0.1 && std::fabs(rb::wrap(foot.body - body_before)) > 1e-6;
                if (turning && std::isnan(turn_from)) turn_from = body_before;
                if (!turning && !std::isnan(turn_from)) {
                    const double a = std::fabs(rb::wrap(foot.body - turn_from));
                    if (a > 4.0) { ++turns; turns_big += a > 45.0; turn_big = std::max(turn_big, a); }
                    turn_from = std::nan("");
                }
            }
            const int k = std::min(N - 1, static_cast<int>((t0 + t) * 30));
            double px[6], py[6];
            int fr[6] = {};
            for (int i = 0; i < NP; ++i) at(i, k, px[i], py[i], &fr[i]);
            if (t0 + t <= 120.0) {   // the lane's view (the take's 120 s): the people are at their own frames' bones
                BoneRow B{};
                for (int i = 0; i < 5; ++i) for (int j = 0; j < 21; ++j) B[i * 21 + j] = bones[fr[i]][i * 21 + j];
                if (seen_np == 6) for (int j = 0; j < 21; ++j) B[105 + j] = bones[fr[5]][105 + j];
                const int kt = std::min(N - 1, static_cast<int>(std::lround((t0 + t) * 30)));
                const int n = seen_points(foot.x, foot.y, foot.body, g_fp_camera ? cam : fp_yaw, pitch, D ? 1.0 : ease((kt - kTakeoverTick) / double(kEaseTicks)), px, py, B, seen_np);
                fp_w += dt; fp_half += (n >= 4) * dt; fp_any += (n >= 1) * dt;
                run_bin[std::min(23, static_cast<int>((t0 + t) / 5))] += (n >= 4) * dt;
                if (n >= 4) gap = 0; else { gap += dt; if (gap > gap_max) { gap_max = gap; gap_at = t0 + t - gap; } }
                if (std::getenv("FPDUMP")) std::fprintf(stderr, "FP %u %.3f %d %d %.1f %.1f %.2f\n", seed, t0 + t, n, held.walks() ? 1 : 0, foot.body, cam, foot.v);
            }
            // the rules, as the take evaluates them: after the first key, an approach from out of reach
            for (size_t r = 0; r < (D ? rule_count : rule_count - 1); ++r) {
                const double d = std::hypot(foot.x - px[rules[r].subject], foot.y - py[rules[r].subject]);
                if (d >= rules[r].metres) armed[r] = true;
                if (!acted || !armed[r] || d >= rules[r].metres || fired[rules[r].then[0]] >= 0) continue;
                for (int j : rules[r].then) if (j >= 0) { fired[j] = k; delay[j] = foot.uni(5.0, 6.5); }
                char b[160];
                std::snprintf(b, sizeof b, "%.1f s %s (%.2f m)", t0 + t, rules[r].id, d);
                events.push_back(b);
            }
            rb::WalkView v;
            v.t = t; v.dt = dt; v.x = foot.x; v.y = foot.y; v.heading = foot.body; v.cam_yaw = cam; v.cam_pitch = pitch;
            for (int i = 0; i < NP; ++i) v.people.push_back({ids[i], px[i], py[i]});
            const rb::Input in = use_walker ? stroller.step(v) : walker.step(v);
            held = in.keys; mdx = in.mouse_dx; mdy = in.mouse_dy; body_to = in.body_to;
            const bool walks = held.walks();
            walked_m += foot.v * dt;
            acted = acted || walks;
            if (walks && !was_w && verbose) { char b[200]; std::snprintf(b, sizeof b, "%.1f s %s down at (%.2f,%.2f), body %.0f off the keys' way, camera %.0f off the body", t0 + t, key_name(held), foot.x, foot.y, std::fabs(rb::wrap(cam + rb::key_way(held) - foot.body)), rb::wrap(cam - foot.body)); events.push_back(b); }
            if (!walks && was_w && verbose) { char b[160]; std::snprintf(b, sizeof b, "%.1f s keys up at (%.2f,%.2f)", t0 + t, foot.x, foot.y); events.push_back(b); }
            if (walks && !was_w) { press_at = t; ++presses; ++key_uses[key_index(held)]; }
            if (!walks && was_w) { press_min = std::min(press_min, t - press_at); walks_n += t - press_at >= kWalkHeldS; }
            was_w = walks;
            if (pathlog) std::fprintf(stderr, "%u %.3f %.3f %.3f %.1f %.1f %d %.3f %.3f %.3f %.3f %.3f %.3f %.3f %.3f %.3f %.3f %.2f\n", seed, t0 + t, foot.x, foot.y, foot.body, cam, walks ? 1 : 0,
                                      px[0], py[0], px[1], py[1], px[2], py[2], px[3], py[3], px[4], py[4], foot.v);
            if (!walks && foot.v < 0.1) { stand += dt; if (stand > stand_max) { stand_max = stand; stand_at = t0 + t - stand; } } else stand = 0;
            if (t < 2.0) continue;
            if (walks) ++walking;
            ++frames;
            if (std::fabs(rb::wrap(cam - foot.body)) <= 60.0) ++behind;
            for (int i = 0; i < NP; ++i) {
                const double d = std::hypot(foot.x - px[i], foot.y - py[i]);
                if (d < close[i]) { close[i] = d; close_t[i] = t0 + t; }
            }
        }
        // the first-person view's turn: the camera's yaw unwrapped, smoothed as stage_tape smooths the lane
        // (Gaussian, FP_SMOOTH_TICKS = 6), differentiated frame to frame
        double over90 = 0, over60 = 0, p95 = 0, acc95 = 0, rmax = 0, jerk95 = 0;
        if (views.size() > 8) {
            // on the tick grid, as the tape has it: a skipped tick (a 1/15 s step) between its neighbours
            std::vector<double> raw(views.size()), at(views.size());
            raw[0] = views[0].second; at[0] = views[0].first;
            for (size_t i = 1; i < views.size(); ++i) {
                raw[i] = raw[i - 1] + rb::wrap(views[i].second - views[i - 1].second);
                at[i] = at[i - 1] + views[i].first;
            }
            std::vector<double> yaw;
            for (size_t i = 0; ; ) {
                const double tk = at[0] + yaw.size() / 30.0;
                while (i + 1 < at.size() && at[i + 1] < tk) ++i;
                if (i + 1 >= at.size()) break;
                const double f = (tk - at[i]) / (at[i + 1] - at[i]);
                yaw.push_back(raw[i] + f * (raw[i + 1] - raw[i]));
            }
            views.assign(yaw.size(), {1.0 / 30.0, 0.0});
            std::vector<double> sm(yaw.size());
            for (size_t i = 0; i < yaw.size(); ++i) {
                double w = 0, a = 0;
                for (int j = -18; j <= 18; ++j) {
                    const long q = static_cast<long>(i) + j;
                    if (q < 0 || q >= static_cast<long>(yaw.size())) continue;
                    const double g = std::exp(-0.5 * j * j / 36.0);
                    w += g; a += g * yaw[static_cast<size_t>(q)];
                }
                sm[i] = a / w;
            }
            std::vector<double> rates, accs, jerks;
            double last_rate = 0, last_acc = 0;
            for (size_t i = 1; i < sm.size(); ++i) {
                const double r = std::fabs(sm[i] - sm[i - 1]) / views[i].first;
                rates.push_back(r);
                if (i > 1) {
                    const double acc = std::fabs(r - last_rate) / views[i].first;
                    if (i > 2) jerks.push_back(std::fabs(acc - last_acc) / views[i].first);
                    accs.push_back(acc);
                    last_acc = acc;
                }
                last_rate = r;
                if (r > 90.0) over90 += views[i].first;
                if (r > 60.0) over60 += views[i].first;
                rmax = std::max(rmax, r);
            }
            auto pct = [](std::vector<double> v, double q) { std::sort(v.begin(), v.end()); return v[static_cast<size_t>(q * (v.size() - 1))]; };
            p95 = pct(rates, 0.95); acc95 = pct(accs, 0.95); jerk95 = pct(jerks, 0.95);
        }
        over90_sum += over90; over90_max = std::max(over90_max, over90); over60_sum += over60; p95_max = std::max(p95_max, p95);
        acc95_max = std::max(acc95_max, acc95); rate_max = std::max(rate_max, rmax); turn_max = std::max(turn_max, turn_big);
        jerk95_max = std::max(jerk95_max, jerk95);
        turns_big_max = std::max(turns_big_max, turns_big); turns_sum += turns;
        // what a take must be: both story rules, never the jogger's; nobody touched
        const double needC[6] = {0.8, 0.9, 0.72, 1.5, 1.2, 0.0}, needD[6] = {1.5, 1.5, 1.5, 1.5, 1.5, 1.5};
        // C_TOURIST_ONLY=1: C's event is the tourist stopped (the user, 2026-10-15: C blocks her and keeps off the pair)
        bool ok = (D ? fired[0] < 0 && fired[2] < 0 && fired[5] < 0 : tourist_only ? fired[0] < 0 && fired[2] >= 0 : fired[0] >= 0 && fired[2] >= 0) && fired[3] < 0 &&
                  (use_walker || (walker.held_back() == 0 && walker.arrived() == stops));   // stuck short of a spot is a failed take too
        for (int i = 0; i < NP; ++i) { ok = ok && close[i] >= (D ? needD : needC)[i]; worst[i] = std::min(worst[i], close[i]); }
        // the first-person lane over the whole 120 s (the takeover's 11 s are the same in every run) and after the takeover
        const double fp_all = D ? 100.0 * fp_half / fp_w : 100.0 * (pre_half + fp_half) / (pre_w + fp_w), fp_post = 100.0 * fp_half / std::max(fp_w, 1e-9);
        const double any_all = 100.0 * (pre_any + fp_any) / std::max(pre_w + fp_w, 1e-9);
        if (true) { ok = ok && fp_all >= fp_need_all && fp_post >= fp_need_post; fp_all_sum += fp_all; fp_post_sum += fp_post; fp_all_min = std::min(fp_all_min, fp_all); fp_post_min = std::min(fp_post_min, fp_post);
                  gap_worst = std::max(gap_worst, gap_max); stand_worst = std::max(stand_worst, stand_max); press_worst = std::min(press_worst, press_min);
                  behind_min = std::min(behind_min, 100.0 * behind / frames); behind_sum += 100.0 * behind / frames;
                  for (int b = 0; b < 24; ++b) binsum[b] += pre_binhalf[b] + run_bin[b]; }
        // the requirement's limits on every lane's turn (engine_qc config: p95 90 deg/s and never over 90, acceleration p95
        // 150 deg/s^2, jerk p95 600 deg/s^3), the first person's too; TURN_LIMITS=0 leaves them out
        const bool turn_ok = !turn_limits || (rmax <= 90.0 && p95 <= 90.0 && acc95 <= 150.0 && jerk95 <= 600.0);
        ok = ok && turn_ok;
        // a take that stands about or hardly walks is no take either (the user: C's second half not standing about):
        // STAND_MAX_S, the longest stand allowed; MIN_WALKS, the walks a run must make (0: either unchecked)
        if (stand_max_s > 0 && stand_max > stand_max_s) ok = false;
        if (min_walks > 0 && walks_n < min_walks) ok = false;
        if (!use_walker && start_max >= 0 && walker.start_turn() > start_max) ok = false;
        if (!use_walker && taps_max >= 0 && walker.taps() > taps_max) ok = false;
        if (!ok) ++bad_runs;
        bool other_keys = false;
        for (int i = 0; i < 8; ++i) { key_total[i] += key_uses[i]; other_keys = other_keys || (i != 0 && key_uses[i] > 0); }
        runs_w_only += presses > 0 && !other_keys;
        walks_min = std::min(walks_min, walks_n); walked_min = std::min(walked_min, walked_m);
        std::printf("seed %u%s: rules %s | spots %zu/%zu missed %d held %d | start turn max %.0f | behind %.1f%% fp %.1f%% (post %.1f%%, any %.1f%%, gap max %.1f s at %.0f) stand max %.1f s at %.0f | W %d presses min %.2f s | walking %.0f%% |",
                    seed, ok ? "" : " BAD", D ? (fired[0] < 0 && fired[2] < 0 && fired[3] < 0 && fired[5] < 0 ? "none" : "FIRED") : tourist_only ? (fired[0] < 0 && fired[2] >= 0 ? "tourist" : "WRONG") : fired[0] >= 0 && fired[2] >= 0 ? "both" : "MISSING", walker.arrived(), stops, walker.missed(), walker.held_back(),
                    walker.start_turn(), 100.0 * behind / frames, fp_all, fp_post, any_all, gap_max, gap_at, stand_max, stand_at, presses, press_min, 100.0 * walking / frames);
        for (int i = 0; i < NP; ++i) std::printf(" %s %.2f (%.0f)", ids[i], close[i], close_t[i]);
        std::printf(" | walks %d, %.1f m | taps %d | keys", walks_n, walked_m, use_walker ? 0 : walker.taps());   // "W n presses" above counts every press, taps too
        for (int i = 0; i < 8; ++i) if (key_uses[i]) std::printf(" %s %d", kKeyNames[i], key_uses[i]);
        std::printf(" | view turn%s >90 deg/s %.1f s, >60 %.1f s, p95 %.0f, max %.0f deg/s, accel p95 %.0f, jerk p95 %.0f | standing turns %d (%d over 45 deg, biggest %.0f)\n",
                    turn_ok ? "" : " OVER", over90, over60, p95, rmax, acc95, jerk95, turns, turns_big, turn_big);
        if (verbose) for (const auto& e : events) std::printf("     %s\n", e.c_str());
    }
    std::printf("%d/%d runs bad; closest over all runs:", bad_runs, seeds);
    for (int i = 0; i < NP; ++i) std::printf(" %s %.2f", ids[i], worst[i]);
    std::printf("\n");
    std::printf("first person, half a person in view: whole take mean %.1f%% min %.1f%% | after the takeover mean %.1f%% min %.1f%% | longest gap %.1f s | longest stand %.1f s | shortest W %.2f s | behind mean %.1f%% min %.1f%%\n",
                        fp_all_sum / seeds, fp_all_min, fp_post_sum / seeds, fp_post_min, gap_worst, stand_worst, press_worst, behind_sum / seeds, behind_min);
    std::printf("first-person view turn: over 90 deg/s mean %.1f s, worst %.1f s | over 60 mean %.1f s | p95 worst %.0f deg/s | max %.0f deg/s | accel p95 worst %.0f deg/s^2 | jerk p95 worst %.0f deg/s^3 | standing turns mean %.1f, over 45 deg at most %d, biggest %.0f deg\n",
                over90_sum / seeds, over90_max, over60_sum / seeds, p95_max, rate_max, acc95_max, jerk95_max, double(turns_sum) / seeds, turns_big_max, turn_max);
    std::printf("keys each walk went on, over all runs:");
    for (int i = 0; i < 8; ++i) std::printf(" %s %ld", kKeyNames[i], key_total[i]);
    std::printf(" | runs on W alone %d | walks at least %d, %.1f m\n", runs_w_only, walks_min, walked_min);
    if (std::getenv("FPMAP")) {
        std::printf("visible by 5 s (mean over runs, %%):");
        for (int b = 0; b < 24; ++b) std::printf(" %d:%.0f", b * 5, 100.0 * binsum[b] / (5.0 * seeds));
        std::printf("\n");
    }
    return bad_runs ? 1 : 0;
}
