// Stage plan contract: parse, resolve and evaluate a small cast on synthetic
// clips whose poses are known in closed form, so every property the game relies
// on can be checked without the game.
#include "stage_plan.hpp"
#include "stage_take.hpp"   // advance_axis: the free camera's motion law, portable
#include "stage_robot.hpp"  // the program that plays B, C and D, portable

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <fstream>
#include <iostream>
#include <random>
#include <sstream>
#include <string>

using namespace diogenes::gta::stage;

namespace {

constexpr double kPi = 3.14159265358979323846;
// How the game turns a standing person on the spot when the take asks it to (stage_take hold_view),
// measured on beach-demo-003 D (2026-10-09, 26 turns): half a second to a second whatever the angle --
// 40 degrees at about 80 deg/s, 90 at 130-190, 180 at 170-180 (peaks over 200) -- setting off a moment
// after the ask: the faster the more is left, kBodyTurnMin + kBodyTurnGain x what is left, up to kBodyTurnMax.
constexpr double kBodyTurnMin = 40.0, kBodyTurnGain = 1.5, kBodyTurnMax = 220.0, kBodyTurnDelay = 0.2;
double body_turn(double off, double dt) {
    const double rate = std::min(kBodyTurnMax, kBodyTurnMin + kBodyTurnGain * std::fabs(off));
    return std::clamp(off, -rate * dt, rate * dt);
}

// Bones: 0 Spine3, 1 Head, 2 L_Hand, 3 R_Hand, 4 L_Foot, 5 R_Foot.
void set_pose(ClipCal& c, int j, double swing, double hands_up) {
    const double p[kPoseBones][3] = {
        {0, 0, 0.45},  {0, 0.02, 0.7},
        {-0.25, -swing, 0.05 + hands_up}, {0.25, swing, 0.05 + hands_up},
        {-0.1, swing, -0.9}, {0.1, -swing, -0.9},
    };
    for (int b = 0; b < kPoseBones; ++b)
        for (int k = 0; k < 3; ++k) c.pose[static_cast<size_t>(c.pose_at(j, b, k))] = p[b][k];
}
ClipCal make_clip(const std::string& id, int n) {
    ClipCal c;
    c.id = id; c.n = n; c.duration_s = n / 30.0; c.zoff = 0.99;
    c.root_x.assign(static_cast<size_t>(n), 0.0);
    c.root_y.assign(static_cast<size_t>(n), 0.0);
    c.yaw.assign(static_cast<size_t>(n), 0.0);
    c.pose.assign(static_cast<size_t>(n) * kPoseBones * 3, 0.0);
    return c;
}

// A walk: 3.6 m per 60-tick cycle, legs crossing (feet together) at j = 0 and 30.
ClipCal walk_clip() {
    ClipCal c = make_clip("walk", 60);
    c.speed = 3.6 / 2.0;
    for (int j = 0; j < 60; ++j) {
        c.root_y[static_cast<size_t>(j)] = 3.6 * (j + 0.5) / 60.0;
        set_pose(c, j, 0.35 * std::sin(2 * kPi * j / 60.0), 0.0);
    }
    return c;
}
// An idle: feet together, a slow sway of the hands, drifting 1 cm.
ClipCal idle_clip(const std::string& id, int n, double hands_up) {
    ClipCal c = make_clip(id, n);
    for (int j = 0; j < n; ++j) {
        c.root_x[static_cast<size_t>(j)] = 0.01 * std::sin(2 * kPi * j / n);
        set_pose(c, j, 0.03 * std::sin(2 * kPi * j / n), hands_up);
    }
    return c;
}
// Putting a phone to the ear (up) or away (down): feet together throughout.
ClipCal phone_move(const std::string& id, bool up) {
    ClipCal c = make_clip(id, 40);
    for (int j = 0; j < 40; ++j) set_pose(c, j, 0.0, 0.35 * (up ? j / 39.0 : 1.0 - j / 39.0));
    return c;
}
// One half of a hand-over: stands 0.46 m to one side facing the other, starts
// and ends at rest, hands up in the middle.
ClipCal pair_clip(const std::string& id, double side, double yaw) {
    ClipCal c = make_clip(id, 100);
    for (int j = 0; j < 100; ++j) {
        c.root_x[static_cast<size_t>(j)] = side;
        c.yaw[static_cast<size_t>(j)] = yaw;
        set_pose(c, j, 0.0, 0.3 * std::sin(kPi * j / 99.0));
    }
    return c;
}

// The body a tick puts on screen: origin plus the clip's root offset, rotated.
void body_at(const Plan& plan, const Calibration& cal, int actor, int tick, double& x, double& y) {
    const TickOp op = tick_op(plan, cal, actor, tick);
    const ResolvedBeat& r = plan.actors[static_cast<size_t>(actor)].beats[static_cast<size_t>(op.beat)];
    const ClipCal& c = *cal.clip(r.clip);
    const int j = std::min(c.n - 1, static_cast<int>(op.phase * c.n));
    const double h = op.heading * kPi / 180.0;
    x = op.ox + c.root_x[static_cast<size_t>(j)] * std::cos(h) - c.root_y[static_cast<size_t>(j)] * std::sin(h);
    y = op.oy + c.root_x[static_cast<size_t>(j)] * std::sin(h) + c.root_y[static_cast<size_t>(j)] * std::cos(h);
}

const char* kSpec = R"(dm-gta-stage-spec/1
# the beach stage from the rehearsal probes
frame -1566.4618 -1144.0144 1.87 104.97
ticks 3600
transition blend 4
clip walk move_m@generic walk walk
clip idle amb@world_human_stand_impatient@male@no_sign@base base loop
clip phone amb@world_human_stand_mobile@male@standing@call@base base loop
clip phone_in amb@world_human_stand_mobile@male@standing@call@enter enter once
clip phone_out amb@world_human_stand_mobile@male@standing@call@exit exit once
clip give_a mp_common givetake1_a once
clip give_b mp_common givetake1_b once
pair give give_a give_b -2.0 1.4 0
path down 0 1.0 2.6 180   # facing -y on stage
line down 3.0
arc down 0.4 -180
line down 1.0
path up 0 1.8 -1.0 0
line up 3.0
arc up 0.4 -180
line up 1.0
path stroll 0 3.0 -2.0 0
line stroll 2.4
path ra 0 -4.0 -3.0 90
line ra 2.0
path rb 0 0 0 90
line rb 2.0
actor pacer a_m_y_hipster_01
look pacer 3 0 1
prop pacer prop_npc_phone_02 28422
beat pacer idle phone 4.0 at 1.0 2.6 180
beat pacer once phone_out
beat pacer walk walk down
beat pacer once phone_in
beat pacer idle phone 4.0
beat pacer once phone_out
beat pacer walk walk up
beat pacer once phone_in
repeat pacer 0
actor chat_a a_m_y_beach_01
transition chat_a snap
beat chat_a idle idle 10.0
beat chat_a pair give a
repeat chat_a 0
actor chat_b a_f_y_beach_01
beat chat_b idle idle 10.0
beat chat_b pair give b
repeat chat_b 0
actor stroller a_m_y_beach_02
beat stroller walk walk stroll
beat stroller idle idle 2.0
repeat stroller 0
actor rambler a_m_y_beach_02
beat rambler walk walk ra
beat rambler walk walk rb
beat rambler idle idle 3.0
repeat rambler 0
rule grab near chat_a 1.5 then chat_a:handsup,chat_b:flee say 玩家逼近递东西的两人 (#958)：递东西的人举手，接东西的人逃开
rule shove touch pacer then pacer:watch say 玩家撞到打电话的人，他停下看着玩家
)";

// A calibration for any spec, from the clip kinds alone: walks, idles, once
// clips and the two halves of each pair, each with poses of the kinds above.
Calibration synthetic_calibration(const StageSpec& spec) {
    Calibration cal;
    for (const auto& c : spec.clips) {
        bool half_a = false, half_b = false;
        for (const auto& p : spec.pairs) { half_a = half_a || p.clip_a == c.id; half_b = half_b || p.clip_b == c.id; }
        ClipCal k;
        if (c.kind == ClipKind::Walk) k = walk_clip();
        else if (half_a) k = pair_clip(c.id, -0.46, 90.0);
        else if (half_b) k = pair_clip(c.id, 0.46, -90.0);
        else if (c.kind == ClipKind::Once) k = phone_move(c.id, c.id.find("out") == std::string::npos);
        else k = idle_clip(c.id, 80 + static_cast<int>(cal.clips.size()) * 7, 0.1);
        k.id = c.id;
        cal.clips.push_back(k);
    }
    return cal;
}

// The free camera as the take flies it: one axis driven by keys that go on and
// off and a jittery mouse, on frames of 4-7 ms with the odd capped 1/15 s step,
// sampled on the 30 Hz tick boundaries (sub-stepped to them, as the take does).
// Returns the worst speed, acceleration and jerk those samples show.
void fly_axis(bool angular, unsigned seed, double limit_rate, double limit_acc, double limit_jerk, double worst[3]) {
    std::mt19937 rng(seed);
    auto u = [](std::mt19937& g) { return robot::uniform(g, 0.0, 1.0); };   // the same draws on every compiler
    FlightAxis s;
    std::vector<double> x;
    double t = 0, next = 0;
    while (t < 90.0) {
        const double dt = u(rng) < 0.02 ? 1.0 / 15.0 : 0.004 + 0.003 * u(rng);
        const int phase = static_cast<int>(t / 1.5) % 4;
        double target = angular ? (phase == 0 ? 70.0 : phase == 2 ? -90.0 : 0.0) + (u(rng) - 0.5) * 80.0
                                : (phase == 0 ? 4.0 : phase == 2 ? -4.0 : 0.0);
        const double end = t + dt;
        while (next <= end) {
            advance_axis(s, target, next - t, limit_rate, limit_acc, limit_jerk);
            t = next;
            x.push_back(s.x);
            next += 1.0 / 30.0;
        }
        advance_axis(s, target, end - t, limit_rate, limit_acc, limit_jerk);
        t = end;
    }
    const double h = 1.0 / 30.0;
    worst[0] = worst[1] = worst[2] = 0;
    for (size_t i = 3; i < x.size(); ++i) {
        worst[0] = std::max(worst[0], std::fabs(x[i] - x[i - 1]) / h);
        worst[1] = std::max(worst[1], std::fabs(x[i] - 2 * x[i - 1] + x[i - 2]) / (h * h));
        worst[2] = std::max(worst[2], std::fabs(x[i] - 3 * x[i - 1] + 3 * x[i - 2] - x[i - 3]) / (h * h * h));
    }
}

// Every tick of every actor in exactly one beat, in order; every hand-over one
// 'a' and one 'b' half on the same tick; nothing unrepresentable in the plan.
int check_plan(const StageSpec& spec, const Plan& plan) {
    for (const auto& ap : plan.actors) {
        int t = 0;
        for (const auto& r : ap.beats) {
            if (r.start != t || r.ticks <= 0) return 1;
            t += r.ticks;
        }
        if (t != spec.ticks) return 2;
    }
    for (const auto& ap : plan.actors)
        for (const auto& r : ap.beats) {
            if (r.kind != BeatKind::Pair) continue;
            int halves = 0;
            for (const auto& ak : plan.actors)
                for (const auto& bk : ak.beats) halves += bk.kind == BeatKind::Pair && bk.start == r.start && bk.pair_index == r.pair_index;
            if (halves != 2) return 3;
        }
    std::istringstream words(render_plan(plan));
    for (std::string w; words >> w;)
        if (w == "nan" || w == "-nan" || w == "inf" || w == "-inf") return 4;
    return 0;
}

}  // namespace

int main(int argc, char** argv) {
    StageSpec spec;
    std::string err = parse_stage_spec(kSpec, spec);
    if (!err.empty()) { std::cerr << "parse: " << err << "\n"; return 1; }
    if (spec.actors.size() != 5 || spec.paths.size() != 5 || spec.pairs.size() != 1 || spec.ticks != 3600) return 2;
    if (spec.blend_in != 4.0 || spec.actors[0].blend_in != -1 || spec.actors[1].blend_in != 0) return 24;
    if (spec.rules.size() != 2 || spec.rules[0].then.size() != 2 || spec.rules[0].then[1].second != "flee" || spec.rules[0].metres != 1.5 ||
        spec.rules[0].say.find("举手") == std::string::npos || spec.rules[1].when != "touch") return 23;
    // a '#' inside the causal record is text, not a comment
    if (spec.rules[0].say.find("(#958)") == std::string::npos ||
        spec.rules[0].say.find("接东西的人逃开") == std::string::npos) { std::cerr << "say cut: " << spec.rules[0].say << "\n"; return 28; }

    // Refusals: every malformed line names itself, nothing is skipped.
    const char* bad[] = {
        "dm-gta-stage-spec/1\nframe 0 0 0\nticks 3600\n",                                 // short frame
        "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 3600\nfrobnicate 1\n",                   // unknown command
        "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 3600\nactor a m\nbeat a idle nope 3\n",  // unknown clip
        "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 3600\nclip i d c loop\nactor a m\nbeat a idle i 3\n",   // first idle unplaced
        "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 3600\nclip i d c loop\nactor a m\nbeat a idle i 3 at 0 0 0\nbeat a idle i 3 at 1 1 0\n",
        "dm-gta-stage-spec/2\n",
        "dm-gta-stage-spec/1\nframe 0 0 0 nan\nticks 3600\n",
        "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 3600\ntransition blend 0.1\n",                         // blend too slow to finish
        "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 3600\ntransition blend 2\n",                           // longer than the retire delay
        "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 3600\npath p 1 0 0 0\narc p 1.0 0\n",                  // an arc that does not turn
        "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 3600\ntransition nobody snap\n",                      // unknown actor
        "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 3600\ntransition snap 4\n",                           // a snap has no speed
        "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 3600\nactor a m\nbeat a idle i 3 at 0 0 0\nrule r near a 1 then a:dance say x\n",   // unknown action
        "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 3600\nclip i d c loop\nactor a m\nbeat a idle i 3 at 0 0 0\nrule r near a 1 then a:flee\n",   // no causal record
    };
    for (const char* b : bad) {
        StageSpec s;
        if (parse_stage_spec(b, s).empty()) { std::cerr << "accepted bad spec: " << b << "\n"; return 3; }
    }

    Calibration cal;
    cal.clips = {walk_clip(), idle_clip("idle", 90, 0.0), idle_clip("phone", 120, 0.35), phone_move("phone_in", true),
                 phone_move("phone_out", false),
                 pair_clip("give_a", -0.46, 90.0), pair_clip("give_b", 0.46, -90.0)};
    auto ground = [](double x, double y) { return 1.5 + 0.004 * (x + 1566.0) - 0.002 * (y + 1144.0); };

    Plan plan;
    err = resolve_plan(spec, cal, ground, plan);
    if (!err.empty()) { std::cerr << "resolve: " << err << "\n"; return 4; }

    // 1. every tick of every actor belongs to exactly one beat, in order
    for (size_t a = 0; a < plan.actors.size(); ++a) {
        int t = 0;
        for (const auto& r : plan.actors[a].beats) {
            if (r.start != t || r.ticks <= 0) { std::cerr << "gap in " << plan.actors[a].id << " at " << t << "\n"; return 5; }
            t += r.ticks;
        }
        if (t != spec.ticks) return 6;
    }

    // 2. the body never jumps: across every beat boundary it moves no more than
    // one walking step (6 cm), and a walk follows its path exactly
    double worst_jump = 0;
    for (int a = 0; a < static_cast<int>(plan.actors.size()); ++a) {
        double px = 0, py = 0;
        body_at(plan, cal, a, 0, px, py);
        for (int t = 1; t < spec.ticks; ++t) {
            double x = 0, y = 0;
            body_at(plan, cal, a, t, x, y);
            const TickOp op = tick_op(plan, cal, a, t);
            if (op.enter) worst_jump = std::max(worst_jump, std::hypot(x - px, y - py));
            if (op.walking && std::hypot(x - op.want_x, y - op.want_y) > 1e-9) { std::cerr << "off path\n"; return 7; }
            px = x; py = y;
        }
    }
    if (worst_jump > 0.065) { std::cerr << "body jumped " << worst_jump << " m at a snap\n"; return 8; }

    // 3. snaps land on matching poses: a stop after a walk is feet-together
    // (the walk crosses its legs at j = 0 and 30) and costs almost nothing
    int stops = 0;
    for (const auto& ap : plan.actors) {
        for (size_t i = 1; i < ap.beats.size(); ++i) {
            const ResolvedBeat& prev = ap.beats[i - 1];
            const ResolvedBeat& r = ap.beats[i];
            if (prev.kind == BeatKind::Walk && r.kind == BeatKind::Idle && r.start + r.ticks < spec.ticks) {
                ++stops;
                const int j_last = (prev.phase0 + prev.ticks - 1) % 60;
                if (std::min(j_last % 30, 30 - j_last % 30) > 1) { std::cerr << "walk stops mid-stride, j=" << j_last << "\n"; return 9; }
                if (r.entry_pop > 0.05) { std::cerr << ap.id << " stops with a pop of " << r.entry_pop << "\n"; return 29; }
            }
            if (r.entry_pop > 0.45) { std::cerr << ap.id << " beat " << i << " pops " << r.entry_pop << "\n"; return 10; }
        }
    }
    if (stops == 0) { std::cerr << "no walk ever stopped: check 3 checked nothing\n"; return 30; }

    // 3b. a walk followed by a walk carries on: the next tick's phase follows on
    // and the body moves one tick's walking, no freeze and no half step
    {
        const int rambler = 4;
        const auto& beats = plan.actors[static_cast<size_t>(rambler)].beats;
        if (beats.size() < 2 || beats[0].kind != BeatKind::Walk || beats[1].kind != BeatKind::Walk) return 31;
        const int t = beats[1].start;
        const TickOp before = tick_op(plan, cal, rambler, t - 1), after = tick_op(plan, cal, rambler, t);
        const int jb = static_cast<int>(before.phase * 60), ja = static_cast<int>(after.phase * 60);
        double x0 = 0, y0 = 0, x1 = 0, y1 = 0;
        body_at(plan, cal, rambler, t - 1, x0, y0);
        body_at(plan, cal, rambler, t, x1, y1);
        const double step = std::hypot(x1 - x0, y1 - y0), want = walk_clip().speed / 30.0;
        if (ja != (jb + 1) % 60 || std::fabs(step - want) > 1e-6) {
            std::cerr << "walk into walk: phase " << jb << " -> " << ja << ", step " << step << " m (want " << want << ")\n";
            return 32;
        }
    }

    // 3c. the first hand-over: the idle waiting for it ends exactly where the
    // pair takes the body over, however its clip drifts
    for (const int who : {1, 2}) {
        const auto& beats = plan.actors[static_cast<size_t>(who)].beats;
        if (beats.size() < 2 || beats[1].kind != BeatKind::Pair) return 33;
        double x0 = 0, y0 = 0, x1 = 0, y1 = 0;
        body_at(plan, cal, who, beats[1].start - 1, x0, y0);
        body_at(plan, cal, who, beats[1].start, x1, y1);
        if (std::hypot(x1 - x0, y1 - y0) > 1e-6) { std::cerr << "first hand-over jumps " << std::hypot(x1 - x0, y1 - y0) << " m\n"; return 34; }
    }

    // 3d. an idle between two hand-overs keeps its length, so its phase must serve
    // both ends: the pose the last hand-over left and the next one's first pose
    // together -- not its entry alone, which leaves every later hand-over unmatched
    int between = 0;
    for (const int who : {1, 2}) {
        const auto& beats = plan.actors[static_cast<size_t>(who)].beats;
        for (size_t i = 1; i + 1 < beats.size(); ++i) {
            const ResolvedBeat& w = beats[i];
            if (w.kind != BeatKind::Idle || beats[i - 1].kind != BeatKind::Pair || beats[i + 1].kind != BeatKind::Pair) continue;
            const ClipCal& pc = *cal.clip(beats[i - 1].clip);
            const ClipCal& ic = *cal.clip(w.clip);
            const ClipCal& nc = *cal.clip(beats[i + 1].clip);
            const auto both = [&](int j0) { return pose_distance(pc, pc.n - 1, ic, j0) + pose_distance(ic, (j0 + w.ticks) % ic.n, nc, 0); };
            double best = 1e18;
            for (int j0 = 0; j0 < ic.n; ++j0) best = std::min(best, both(j0));
            if (both(w.phase0) > best + 1e-12) { std::cerr << "idle before the hand-over at " << beats[i + 1].start << " picked its phase for one end\n"; return 35; }
            ++between;
        }
    }
    if (between == 0) return 36;

    // 4. the two halves of the hand-over meet on the same tick, each standing
    // where its half starts
    for (size_t i = 0; i < plan.actors[1].beats.size(); ++i) {
        const ResolvedBeat& a = plan.actors[1].beats[i];
        if (a.kind != BeatKind::Pair) continue;
        bool met = false;
        for (const auto& b : plan.actors[2].beats) met = met || (b.kind == BeatKind::Pair && b.start == a.start);
        if (!met) return 11;
    }

    // 5. heights: every scene sits zoff above the ground under it
    for (int a = 0; a < static_cast<int>(plan.actors.size()); ++a)
        for (int t = 0; t < spec.ticks; t += 7) {
            const TickOp op = tick_op(plan, cal, a, t);
            if (std::fabs(op.oz - op.ground - 0.99) > 1e-9) return 12;
        }
    // ... and the two halves of a pair, one scene, get one height between their two
    {
        Calibration c2 = cal;
        for (auto& c : c2.clips) if (c.id == "give_b") c.zoff = 1.01;
        Plan p5;
        if (!resolve_plan(spec, c2, ground, p5).empty()) return 21;
        for (int a : {1, 2})
            for (const auto& r : p5.actors[static_cast<size_t>(a)].beats)
                if (r.kind == BeatKind::Pair && std::fabs(r.oz - r.ground - 1.0) > 1e-9) return 22;
    }

    // 5b. every beat carries how it is entered: the stage's blend, or the actor's own snap
    for (const auto& r : plan.actors[0].beats) if (r.blend_in != 4.0) return 25;
    for (const auto& r : plan.actors[1].beats) if (r.blend_in != 0.0) return 26;
    if (tick_op(plan, cal, 0, plan.actors[0].beats[1].start).blend_in != 4.0) return 27;

    // 6. the same inputs give the same plan to the last digit
    Plan again;
    if (!resolve_plan(spec, cal, ground, again).empty() || render_plan(again) != render_plan(plan)) return 13;

    // 7. game time to ticks: a floor on both sides of the performance's start
    if (tick_of(1000, 1000) != 0 || tick_of(1033, 1000) != 0 || tick_of(1034, 1000) != 1 || tick_of(999, 1000) != -1 ||
        tick_of(967, 1000) != -1 || tick_of(966, 1000) != -2 || tick_of(933, 1000) != -3 || tick_of(1000 + 120000, 1000) != 3600)
        return 14;

    // 8. without the put-away clip, a phone call cannot snap into a walk cleanly
    // whatever the moment -- the plan must show it, not hide it
    {
        std::string raw = kSpec;
        const std::string cut = "beat pacer once phone_out\nbeat pacer walk walk down\n";
        raw.replace(raw.find(cut), cut.size(), "beat pacer walk walk down\n");
        StageSpec s2; Plan p3;
        if (!parse_stage_spec(raw, s2).empty() || !resolve_plan(s2, cal, ground, p3).empty()) return 16;
        if (p3.actors[0].beats[1].entry_pop < 0.6) { std::cerr << "a phone-to-walk snap looked clean\n"; return 17; }
    }

    // 9. an uncalibrated clip is refused, not guessed
    Calibration partial = cal;
    partial.clips.pop_back();
    Plan p2;
    if (resolve_plan(spec, partial, ground, p2).empty()) return 15;

    // 10. the calibration file round-trips exactly, so a take that reads it
    // resolves the plan the measuring take resolved, and a cut file is refused
    {
        Calibration back;
        const std::string text = render_calibration(cal);
        if (!parse_calibration(text, back).empty() || render_calibration(back) != text) return 18;
        Plan p4;
        if (!resolve_plan(spec, back, ground, p4).empty() || render_plan(p4) != render_plan(plan)) return 19;
        Calibration cut;
        if (parse_calibration(text.substr(0, text.size() / 2), cut).empty()) return 20;
    }
    // 10b. the feet on the sand phase by phase (2026-10-17): a calibration with each phase's contact round-trips; a
    // standing beat's origin goes down by its phase's contact, a walk's by its clip's lowest; contact on some phases only
    // is refused
    {
        Calibration cc = cal;
        for (auto& c : cc.clips) { c.contact.clear(); for (int j = 0; j < c.n; ++j) c.contact.push_back(0.01 * ((j % 7) - 2)); }
        Calibration back;
        const std::string text = render_calibration(cc);
        if (!parse_calibration(text, back).empty() || render_calibration(back) != text) return 129;
        Plan pc;
        if (!resolve_plan(spec, cc, ground, pc).empty()) return 129;
        int frames = 0, walks = 0;
        for (size_t a = 0; a < plan.actors.size(); ++a)
            for (int t = 0; t < plan.ticks; t += 7) {
                const TickOp o0 = tick_op(plan, cal, static_cast<int>(a), t), o1 = tick_op(pc, cc, static_cast<int>(a), t);
                if (o0.beat < 0 || o1.beat != o0.beat) continue;
                const ResolvedBeat& r = pc.actors[a].beats[static_cast<size_t>(o1.beat)];
                if (r.seated || r.kind == BeatKind::Pair) continue;
                const ClipCal& c = *cc.clip(r.clip);
                const double want = r.kind == BeatKind::Walk ? c.contact_min()
                                                              : c.contact_at(std::min(c.n - 1, static_cast<int>(o1.phase * c.n)));
                if (std::fabs((o0.oz - o1.oz) - want) > 1e-9) {
                    std::cerr << "contact: actor " << a << " tick " << t << " moved " << (o0.oz - o1.oz) << ", want " << want << "\n";
                    return 130;
                }
                ++(r.kind == BeatKind::Walk ? walks : frames);
            }
        if (frames == 0 || walks == 0) { std::cerr << "contact: " << frames << " standing, " << walks << " walking ticks\n"; return 130; }
        std::istringstream lines(text);
        std::string mixed, l;
        bool cut_one = false;
        while (std::getline(lines, l)) {
            if (!cut_one && l.rfind("  1 ", 0) == 0) { l = l.substr(0, l.find_last_of(' ')); cut_one = true; }
            mixed += l + "\n";
        }
        Calibration m;
        if (!cut_one || parse_calibration(mixed, m).empty()) return 131;
    }

    // 11. a hand-over is one 'a' and one 'b': a third person arriving as 'a' on
    // the same tick is refused, not put into the same place
    {
        std::string raw = kSpec;
        raw += "actor chat_c a_m_y_beach_01\nbeat chat_c idle idle 10.0\nbeat chat_c pair give a\nrepeat chat_c 0\n";
        StageSpec s3; Plan p6;
        if (!parse_stage_spec(raw, s3).empty()) return 37;
        const std::string e = resolve_plan(s3, cal, ground, p6);
        if (e.find("'a'") == std::string::npos) { std::cerr << "three halves: " << e << "\n"; return 38; }
    }

    // 12. the rendered plan (whose hash the gate compares) holds every ground
    // height along every path, not only the two ends
    {
        const Path& p = plan.paths[0];
        const size_t mid = (p.x.size() / 2) / 8 * 8;   // a probed sample
        const double mx = p.x[mid], my = p.y[mid];
        auto bumped = [&](double x, double y) { return ground(x, y) + (std::hypot(x - mx, y - my) < 1e-6 ? 0.01 : 0.0); };
        Plan p7;
        if (!resolve_plan(spec, cal, bumped, p7).empty() || render_plan(p7) == render_plan(plan)) { std::cerr << "a mid-path ground change left the plan text alone\n"; return 39; }
    }

    // 12b. the free camera stays inside the requirement's limits on the 30 Hz
    // grid, worst sample included (#958: p95 6 m/s, 8 m/s^2, 20 m/s^3, 90 deg/s,
    // 150 deg/s^2, 600 deg/s^3) when flown per axis as the take flies it: at
    // nine tenths of the limits, each axis at 1/sqrt(3) (moving) or 1/sqrt(2)
    // (turning) of the vector's acceleration and jerk
    for (const unsigned seed : {7u, 11u, 99u}) {
        double w[3];
        fly_axis(false, seed, 0.9 * 6.0, 0.9 * 8.0 / std::sqrt(3.0), 0.9 * 20.0 / std::sqrt(3.0), w);
        if (w[0] > 6.0 || w[1] * std::sqrt(3.0) > 8.0 || w[2] * std::sqrt(3.0) > 20.0) {
            std::cerr << "free camera moves at " << w[0] << " m/s, " << w[1] << " m/s2, " << w[2] << " m/s3 per axis\n";
            return 45;
        }
        fly_axis(true, seed, 0.9 * 90.0, 0.9 * 150.0 / std::sqrt(2.0), 0.9 * 600.0 / std::sqrt(2.0), w);
        if (w[0] > 90.0 || w[1] * std::sqrt(2.0) > 150.0 || w[2] * std::sqrt(2.0) > 600.0) {
            std::cerr << "free camera turns at " << w[0] << " deg/s, " << w[1] << " deg/s2, " << w[2] << " deg/s3 per axis\n";
            return 46;
        }
    }

    // 13. the demo stage (CTest passes its path): it parses, resolves on clips of
    // its kinds, covers every tick and meets every hand-over
    if (argc > 1) {
        std::ifstream f(argv[1], std::ios::binary);
        std::stringstream text;
        text << f.rdbuf();
        StageSpec demo;
        if (!f || !(err = parse_stage_spec(text.str(), demo)).empty()) { std::cerr << "demo: " << err << "\n"; return 40; }
        const Calibration dcal = synthetic_calibration(demo);
        auto flat = [](double, double) { return 1.87; };
        Plan dplan, dagain;
        if (!(err = resolve_plan(demo, dcal, flat, dplan)).empty()) { std::cerr << "demo: " << err << "\n"; return 41; }
        if (const int issue = check_plan(demo, dplan)) { std::cerr << "demo plan check " << issue << "\n"; return 42; }
        if (!resolve_plan(demo, dcal, flat, dagain).empty() || render_plan(dagain) != render_plan(dplan)) return 43;
        if (demo.rules.size() != 5 || demo.rules[0].say.find("两人停下") == std::string::npos) return 44;
    }

    // 14. the program that plays B, C and D (stage_robot.hpp), against models: the
    // free camera flown as fly() flies it, a person who walks where the camera
    // looks. The mouse's scale is the model's secret: the program calibrates.
    {
        namespace rb = diogenes::gta::stage::robot;
        auto cast = [](double t) {   // six people round the stage, four on loops (the jogger's reaches y = -2.4)
            std::vector<rb::Person> out = {{"chat_a", -2.8, 1.4}, {"sitter", 3.4, 0.2}};
            const double loops[4][4] = {{2.0, -1.0, 2.0, 0.5}, {3.0, 1.0, 1.0, -0.35}, {4.0, 0.0, 1.6, 0.3}, {2.5, 0.5, 3.0, 0.45}};
            const char* ids[4] = {"pacer", "tourist", "jogger", "chat_b"};
            for (int i = 0; i < 4; ++i) {
                const double a = loops[i][3] * t + i;
                out.push_back({ids[i], loops[i][1] + loops[i][0] * std::cos(a), loops[i][2] + loops[i][0] * std::sin(a)});
            }
            return out;
        };
        const double a3 = 7.2 / std::sqrt(3.0), j3 = 18.0 / std::sqrt(3.0), a2 = 135.0 / std::sqrt(2.0), j2 = 540.0 / std::sqrt(2.0);
        // at the frame rate a take records at (30, the recorder's cap) and at 60 (uncapped)
        for (const int fps : {30, 60})
        for (unsigned seed : {1u, 2u, 3u, 4u}) {
            rb::Flyer flyer(seed, 120.0);
            FlightAxis px, py, pz, yaw, pitch;
            px.x = 0; py.x = -8; pz.x = 1.7; yaw.x = 0; pitch.x = -5;
            std::mt19937 hitch(seed);
            rb::Keys held;
            int mdx = 0, mdy = 0, frames = 0, framed = 0, moving = 0;
            double t = 0, gap = 1e9, far_y = -1e9, reach = 0, low = 1e9, high = -1e9;
            while (t < 120.0) {
                const double dt = hitch() % 50 == 0 ? 1.0 / 15 : 1.0 / fps;
                // the game: last frame's counts and keys (the look normal is 0.03 a count, as fly() reads it)
                const double wy = -(mdx * 0.03) * 8.0 / dt, wp = -(mdy * 0.03) * 8.0 / dt;
                const double f = (held.w ? 1.0 : 0.0) - (held.s ? 1.0 : 0.0), r = (held.d ? 1.0 : 0.0) - (held.a ? 1.0 : 0.0);
                const double u = (held.e ? 1.0 : 0.0) - (held.q ? 1.0 : 0.0), h = yaw.x * kPi / 180.0;
                double hx = -std::sin(h) * f + std::cos(h) * r, hy = std::cos(h) * f + std::sin(h) * r;
                const double hn = std::hypot(hx, hy);
                if (hn > 1e-9) { hx /= hn; hy /= hn; }
                advance_axis(px, hx * rb::FlyRoom{}.speed, dt, 5.4, a3, j3);
                advance_axis(py, hy * rb::FlyRoom{}.speed, dt, 5.4, a3, j3);
                advance_axis(pz, u * rb::FlyRoom{}.climb, dt, 5.4, a3, j3);
                advance_axis(yaw, std::clamp(wy, -81.0, 81.0), dt, 81.0, a2, j2);
                advance_axis(pitch, std::clamp(wp, -81.0, 81.0), dt, 81.0, a2, j2);
                t += dt;
                rb::FlyView v;
                v.t = t; v.dt = dt; v.x = px.x; v.y = py.x; v.z = pz.x; v.yaw = yaw.x; v.pitch = pitch.x;
                v.people = cast(t);
                for (const auto& p : v.people) { v.fx += p.x / v.people.size(); v.fy += p.y / v.people.size(); }
                const rb::Input in = flyer.step(v);
                held = in.keys; mdx = in.mouse_dx; mdy = in.mouse_dy;
                if (t < 2.0) continue;   // calibrating
                ++frames;
                for (const auto& p : v.people) gap = std::min(gap, rb::gap_to(v.x, v.y, v.z, p.x, p.y));
                far_y = std::max(far_y, v.y);
                reach = std::max(reach, std::hypot(v.x, v.y - 1.6));
                low = std::min(low, v.z); high = std::max(high, v.z);
                if (std::fabs(rb::wrap(rb::heading_of(v.fx - v.x, v.fy - v.y) - v.yaw)) < 35.0) ++framed;
                if (std::hypot(px.v, py.v) + std::fabs(pz.v) > 0.1) ++moving;
            }
            const rb::Keys used = flyer.used();
            if (!flyer.mouse().ready() || flyer.mouse().k_yaw() >= 0) { std::cerr << "flyer " << seed << " at " << fps << " fps: the mouse did not calibrate\n"; return 60; }
            if (!(used.w && used.a && used.s && used.d && used.q && used.e)) { std::cerr << "flyer " << seed << " at " << fps << " fps: not all of W A S D Q E pressed\n"; return 61; }
            if (gap < 1.5 || far_y > 4.0 || reach > 12.0 || low < 1.3 || high > 5.8) {
                std::cerr << "flyer " << seed << " at " << fps << " fps: out of its room -- gap " << gap << " m, y " << far_y << ", reach " << reach << ", z " << low << ".." << high << "\n";
                return 62;
            }
            if (framed < 0.75 * frames || moving < 0.5 * frames) {
                std::cerr << "flyer " << seed << " at " << fps << " fps: framed " << framed << "/" << frames << ", moving " << moving << "\n";
                return 63;
            }
        }
        // the game's turn in one go, and the glide (2026-10-13) for D's stroll; the changed walk (C with no
        // route, unused since 2026-09-28) gives up its first person to a glide's slower turns -- 75% here --
        // so a C that wanders is simulated before it is given one (tools/route_sim)
        for (const char* turn : {"game", "glide 50 100"})
        for (const int fps : {30, 60})
        for (bool changed : {false, true}) {
            if (changed && std::string(turn) != "game") continue;
            for (unsigned seed : {5u, 6u, 7u}) {
                rb::TurnStyle style;
                rb::parse_turn(turn, style);
                const bool glide = style.mode == rb::TurnMode::Glide;
                rb::Walker walker(seed, changed, 120.0, {{"chat_a", 1.2}, {"tourist", 1.0}, {"jogger", 0.8}}, rb::WalkRoom{}, style);
                double x = 0, y = -6, heading = 0, cam = 0, pitch = -5, t = 0, gap = 1e9, speed = 0;
                std::mt19937 hitch(seed);
                rb::Keys held;
                int mdx = 0, mdy = 0, frames = 0, behind = 0, walking = 0, fp_sees = 0;
                bool other_keys = false;   // anything but W (the customer, 2026-10-16: not W alone)
                double body_to = std::nan(""), turn_wait = 0, walk_run = 0, longest_walk = 0;
                while (t < 120.0) {
                    const double dt = hitch() % 50 == 0 ? 1.0 / 15 : 1.0 / fps;
                    // the game: the camera turns 0.08 degrees a count (right turns right), the person
                    // walks at 1.8 m/s the keys' way off the camera (rb::key_way: W+A 45 degrees left, A 90, S 180)
                    cam = rb::wrap(cam - 0.08 * mdx);
                    pitch = std::clamp(pitch - 0.08 * mdy, -70.0, 45.0);
                    // a GTA person turns to where the keys send it and gets up to walking
                    // speed in about 0.3 s (a short tap turns it round, hardly moves it)
                    if (held.walks()) {
                        // walking, the way follows the camera 0.3 s behind (as the route test's person)
                        const double dir = cam + rb::key_way(held);
                        heading = rb::wrap(heading + rb::wrap(dir - heading) * (1.0 - std::exp(-3.5 * dt)));
                        speed = std::min(1.8, speed + 6.0 * dt);
                    } else {
                        speed = std::max(0.0, speed - 8.0 * dt);
                        // standing, the take turns the body where the program means it to face (hold_view):
                        // the game's turn, or a glide's heading set every frame with no key down
                        const double off = std::isnan(body_to) ? 0.0 : rb::wrap(body_to - heading);
                        turn_wait = speed < 0.3 && std::fabs(off) > 4.0 ? turn_wait + dt : 0.0;
                        if (glide && !std::isnan(body_to)) heading = rb::wrap(body_to);
                        else if (!glide && turn_wait > kBodyTurnDelay) heading = rb::wrap(heading + body_turn(off, dt));
                    }
                    x += -std::sin(heading * kPi / 180.0) * speed * dt;
                    y += std::cos(heading * kPi / 180.0) * speed * dt;
                    t += dt;
                    rb::WalkView v;
                    v.t = t; v.dt = dt; v.x = x; v.y = y; v.heading = heading; v.cam_yaw = cam; v.cam_pitch = pitch;
                    v.people = cast(t);
                    const rb::Input in = walker.step(v);
                    held = in.keys; mdx = in.mouse_dx; mdy = in.mouse_dy; body_to = in.body_to;
                    other_keys |= held.a || held.s || held.d;
                    if (held.q || held.e || (held.w && held.s) || (held.a && held.d)) { std::cerr << "walker " << seed << ": keys that cannot walk\n"; return 65; }
                    if (t < 2.0) continue;
                    ++frames;
                    for (const auto& p : v.people) gap = std::min(gap, std::hypot(x - p.x, y - p.y));
                    if (std::fabs(rb::wrap(cam - heading)) <= 60.0) ++behind;
                    if (held.walks()) ++walking;
                    walk_run = held.walks() ? walk_run + dt : 0.0;
                    longest_walk = std::max(longest_walk, walk_run);
                    // the first person (the body's way, stage_tape) has somebody in its 24 mm view
                    for (const auto& p : v.people)
                        if (std::fabs(rb::wrap(rb::heading_of(p.x - x, p.y - y) - heading)) < 0.95 * rb::kFirstPersonHalfDeg) { ++fp_sees; break; }
                }
                const std::string mode = std::string(changed ? "changed" : "unchanged") + " at " + std::to_string(fps) + " fps, turn " + turn + ",";
                if (!walker.mouse().ready()) { std::cerr << "walker " << mode << " " << seed << ": the mouse did not calibrate\n"; return 64; }
                // the third person as a player plays it: not W alone, and the camera off the person's back now and then
                if (!changed && !other_keys) { std::cerr << "walker " << mode << " " << seed << ": walked on W alone\n"; return 66; }
                if (!changed && behind > 0.9 * frames) { std::cerr << "walker " << mode << " " << seed << ": camera behind " << behind << "/" << frames << "\n"; return 66; }
                if (fp_sees < 0.8 * frames) { std::cerr << "walker " << mode << " " << seed << ": first person sees somebody " << fp_sees << "/" << frames << "\n"; return 74; }
                // long stops facing the cast (WalkRoom); since 2026-10-16 a walk also waits for the camera to come
                // round to its keys' way (5.97% of one take on the game's turn, which walked 6-8% before)
                if (walking < 0.05 * frames || walker.goals_done() < 4) {
                    std::cerr << "walker " << mode << " " << seed << ": walked " << walking << "/" << frames << ", goals " << walker.goals_done() << "\n";
                    return 67;
                }
                if (!changed && gap < 1.25) { std::cerr << "walker unchanged " << seed << ": came within " << gap << " m of somebody\n"; return 68; }
                // a walk is a few metres: one much longer is circling a goal it cannot reach (2026-10-09: 81 s)
                if (longest_walk > 8.0) { std::cerr << "walker " << mode << " " << seed << ": walked " << longest_walk << " s on end\n"; return 75; }
                if (changed && walker.met() < 1) { std::cerr << "walker changed " << seed << ": walked up to nobody\n"; return 69; }
            }
        }
        // Issue #965: a stopped, unchanged stroll must not tap W just to cover
        // the last few centimetres to an arc goal. Exercise public step/goals
        // with calibrated mouse feedback; no private fields or real game needed.
        {
            rb::WalkRoom room;
            room.cx = room.cy = 0.0;
            room.r_min = room.r_max = 6.0;
            room.step_min = room.step_max = 0.0;
            // Every arc goal is at (0, -6 +/- 0.1), independent of the RNG.
            // At y=-5.3 it is 0.6..0.8 m away: outside normal arrival, but too
            // close for a fresh walk. Keep pauses longer than the observation.
            room.pause_min = room.pause_max = 12.0;
            rb::TurnStyle style;
            rb::parse_turn("glide 50 100", style);
            const auto view = [](double y, double heading) {
                rb::WalkView v;
                v.y = y;
                v.heading = v.cam_yaw = heading;
                v.cam_pitch = -5.0;
                v.dt = 1.0 / 30.0;
                return v;
            };
            const auto frame = [](rb::Walker& walker, rb::WalkView& v, rb::Input& previous) {
                // Same sign/scale as the existing robot tests. A standing body
                // follows the requested glide; position is externally supplied.
                v.cam_yaw = rb::wrap(v.cam_yaw - 0.08 * previous.mouse_dx);
                v.cam_pitch = std::clamp(v.cam_pitch - 0.08 * previous.mouse_dy, -70.0, 45.0);
                if (!previous.keys.any() && !std::isnan(previous.body_to))
                    v.heading = rb::wrap(previous.body_to);
                v.t += v.dt;
                previous = walker.step(v);
            };
            {
                rb::Walker walker(7u, false, 120.0, {}, room, style);
                auto v = view(-5.3, 180.0);
                rb::Input previous;
                bool tapped = false;
                for (int f = 0; f < 240; ++f) {
                    frame(walker, v, previous);
                    tapped |= previous.keys.any();
                }
                if (!walker.mouse().ready() || tapped || walker.goals_done() != 1) {
                    std::cerr << "issue965: unchanged near arc goal must be watched without starting W\n";
                    return 170;
                }
            }
            {
                rb::Walker walker(7u, false, 120.0, {}, room, style);
                auto v = view(-3.0, 180.0);   // goal is 2.9..3.1 m away
                rb::Input previous;
                bool walked = false;
                for (int f = 0; f < 240 && !walked; ++f) {
                    frame(walker, v, previous);
                    walked = previous.keys.walks();   // S here: out from the stage, the camera watching it
                }
                if (!walker.mouse().ready() || !walked || walker.goals_done() != 0) {
                    std::cerr << "issue965: a distant unchanged arc goal must still start walking\n";
                    return 171;
                }
                // Introduce an obstacle immediately after the walk starts, before the
                // nominal minimum walk time, where the keys walk it. Safety must still release keys.
                const double yaw = (v.cam_yaw + rb::key_way(previous.keys)) * kPi / 180.0;
                v.people = {{"blocker", v.x - 0.8 * std::sin(yaw), v.y + 0.8 * std::cos(yaw)}};
                frame(walker, v, previous);
                if (previous.keys.any()) {
                    std::cerr << "issue965: obstacle must stop a freshly started walk immediately\n";
                    return 172;
                }
            }
            {
                // A changed walk with only arc goals keeps the original radius.
                rb::Walker walker(7u, true, 120.0, {}, room, style);
                auto v = view(-5.3, 180.0);
                rb::Input previous;
                bool walked = false;
                for (int f = 0; f < 240 && !walked; ++f) {
                    frame(walker, v, previous);
                    walked = previous.keys.walks();
                }
                if (!walker.mouse().ready() || !walked || walker.goals_done() != 0) {
                    std::cerr << "issue965: changed arc goal must keep its original arrival radius\n";
                    return 173;
                }
            }
            {
                // A meeting's own radius is 0.8 * 1.2 = 0.96 m. Standing 1.1 m
                // away must not count as a meeting; only the supplied near pose does.
                rb::Walker walker(7u, true, 120.0, {{"target", 1.2}}, room, style);
                auto v = view(-1.1, 0.0);
                v.people = {{"target", 0.0, 0.0}};
                rb::Input previous;
                for (int f = 0; f < 180; ++f) frame(walker, v, previous);
                if (!walker.mouse().ready() || walker.met() != 0 || walker.goals_done() != 0) {
                    std::cerr << "issue965: meeting radius must not inherit the arc start margin\n";
                    return 174;
                }
                v.y = -0.7;
                frame(walker, v, previous);
                if (walker.met() != 1 || walker.goals_done() != 1 || previous.keys.any()) {
                    std::cerr << "issue965: entering the original meeting radius must still stop and meet\n";
                    return 175;
                }
            }
        }

        // How a standing body turns (the customer, 2026-10-10: turning on the spot was too fast): the text,
        // a glide within its rate and easing that stops where it was asked, and steps the game turns slower
        {
            rb::TurnStyle st;
            if (!rb::parse_turn("", st).empty() || st.mode != rb::TurnMode::Game) return 120;
            if (!rb::parse_turn("steps", st).empty() || st.mode != rb::TurnMode::Steps) return 120;
            if (!rb::parse_turn("glide", st).empty() || st.mode != rb::TurnMode::Glide || st.rate != 45.0 || st.accel != 90.0) return 120;
            if (!rb::parse_turn("glide 50 100", st).empty() || st.rate != 50.0 || st.accel != 100.0 || rb::turn_text(st) != "glide 50 100") return 120;
            if (!rb::parse_turn("walk", st).empty() || st.mode != rb::TurnMode::Walk || st.rate != 60.0) return 120;
            if (!rb::parse_turn("walk 40", st).empty() || st.rate != 40.0 || rb::turn_text(st) != "walk 40") return 120;
            if (!rb::parse_turn("walk 40 straight", st).empty() || st.rate != 40.0 || !st.straight || rb::turn_text(st) != "walk 40 straight") return 120;
            if (!rb::parse_turn("walk straight", st).empty() || st.rate != 60.0 || !st.straight) return 120;
            {   // the walk style: the program never turns the body
                rb::Turner walker_turn(st);
                if (!std::isnan(walker_turn.to(0.0, 1.0 / 30, 0.0, 0.0, 90.0, {}, 0.0, 0.0))) return 120;
            }
            for (const char* wrong : {"fast", "steps 30", "glide 50", "glide 50 x", "glide 120 100", "glide 50 300", "glide 5 100", "glide 50 100 1",
                                      "walk 5", "walk 120", "walk 40 1", "walk x"})
                if (rb::parse_turn(wrong, st).empty()) { std::cerr << "turn accepted: \"" << wrong << "\"\n"; return 120; }
            // a half turn glided at 30 and 60 fps: never over the rate or the easing (the frame that lands on
            // the target eases a little harder, 112 for 100; stage_tape smooths the view over it), on target
            for (const int fps : {30, 60}) {
                rb::parse_turn("glide 50 100", st);
                rb::Turner turner(st);
                const double dt = 1.0 / fps;
                double body = 0.0, rate = 0.0, top = 0.0, jolt = 0.0, t = 0.0;
                for (; t < 8.0; t += dt) {
                    const double to = turner.to(t, dt, body, rate, 180.0 - 1.0, {}, 0.0, 0.0);
                    const double r = rb::wrap(to - body) / dt;
                    jolt = std::max(jolt, std::fabs(r - rate) / dt);
                    top = std::max(top, std::fabs(r));
                    rate = r; body = to;
                }
                if (top > 50.0 + 1e-6 || jolt > 1.15 * 100.0 || std::fabs(rb::wrap(body - 179.0)) > 0.01) {
                    std::cerr << "glide at " << fps << " fps: " << top << " deg/s at most, " << jolt << " deg/s^2, ended at " << body << "\n";
                    return 121;
                }
            }
            // past kSweepDeg it goes round the side with people on it, not the shorter: somebody at heading
            // -100 (to the right), nobody to the left; from 0 to 170 (the shorter way is left)
            {
                rb::parse_turn("glide 50 100", st);
                rb::Turner turner(st);
                const std::vector<rb::Person> people = {{"p", -3.0 * std::sin(-100.0 * kPi / 180.0), 3.0 * std::cos(-100.0 * kPi / 180.0)}};
                double body = 0.0, rate = 0.0, lowest = 0.0;
                for (double t = 0.0; t < 10.0; t += 1.0 / 30) {
                    const double to = turner.to(t, 1.0 / 30, body, rate, 170.0, people, 0.0, 0.0);
                    rate = rb::wrap(to - body) * 30; body = to;
                    lowest = std::min(lowest, body);
                }
                if (lowest > -90.0 || std::fabs(rb::wrap(body - 170.0)) > 0.01) { std::cerr << "glide went round the empty side (lowest " << lowest << ")\n"; return 122; }
            }
            // steps: the game asked 30 degrees at most at a time, each once the last has stopped
            {
                rb::parse_turn("steps", st);
                rb::Turner turner(st);
                double body = 0.0, asked = 0.0, wait = 0.0, rate = 0.0;
                int asks = 0;
                for (double t = 0.0; t < 12.0; t += 1.0 / 30) {
                    const double to = turner.to(t, 1.0 / 30, body, rate, 150.0, {}, 0.0, 0.0);
                    if (std::fabs(rb::wrap(to - asked)) > 1e-9) {
                        if (std::fabs(rb::wrap(to - body)) > rb::kTurnStepDeg + 1.0) { std::cerr << "a step of " << rb::wrap(to - body) << " degrees\n"; return 123; }
                        asked = to; ++asks;
                    }
                    const double off = rb::wrap(asked - body);   // the game's turn (body_turn), after a moment
                    wait = std::fabs(off) > 4.0 ? wait + 1.0 / 30 : 0.0;
                    const double before = body;
                    if (wait > kBodyTurnDelay) body = rb::wrap(body + body_turn(off, 1.0 / 30));
                    rate = rb::wrap(body - before) * 30;
                }
                if (asks < 5 || std::fabs(rb::wrap(body - 150.0)) > 4.0) { std::cerr << "steps: " << asks << " asks, ended at " << body << "\n"; return 123; }
            }
        }
        // C's fixed route (2026-09-28): the text, and walking it among people standing still
        {
            std::vector<rb::RouteStep> steps;
            if (!rb::parse_route("13 -1.84 1.51 5 chat_a; 23 2.05 3.12 5 tourist ; 32.5 -0.2 1.9 14 -;", steps).empty() ||
                steps.size() != 3 || steps[1].look != "tourist" || steps[2].from_s != 32.5 || steps[0].x != -1.84)
                return 89;
            // how a leg is walked, a sixth field: the keys (2026-10-16: any of W A S D and the pairs that walk)
            if (!rb::parse_route("13 1 2 5 a w; 15 1 2 0 - sd", steps).empty() || steps.size() != 2 || steps[1].look != "-" ||
                steps[0].keys != "w" || steps[1].keys != "sd")
                return 89;
            for (const char* keys : {"w", "a", "s", "d", "wa", "wd", "sa", "sd"}) {
                rb::Keys k;
                if (!rb::parse_route(std::string("14 1 2 5 a ") + keys, steps).empty() || !rb::parse_keys(keys, k) ||
                    std::fabs(rb::wrap(rb::key_way(rb::keys_for(rb::key_way(k))) - rb::key_way(k))) > 1e-9)
                    { std::cerr << "route keys " << keys << "\n"; return 89; }
            }
            if (rb::key_way(rb::keys_for(170.0)) != 180.0 || rb::key_way(rb::keys_for(-100.0)) != -90.0 || rb::key_way(rb::keys_for(30.0)) != 45.0)
                return 89;
            // then how: a tap (the keys tapped, standing) or a step; "-" the nearest keys
            if (!rb::parse_route("13 1 2 5 a tap; 13 1 2 5 a d step; 14 1 2 5 a - tap; 15 1 2 5 a sd", steps).empty() || steps.size() != 4 ||
                steps[0].how != rb::RouteStep::How::Tap || !steps[0].keys.empty() || steps[1].how != rb::RouteStep::How::Step ||
                steps[1].keys != "d" || steps[2].how != rb::RouteStep::How::Tap || steps[3].how != rb::RouteStep::How::Walk)
                return 89;
            // or a key timetable instead (2026-10-16: a big turn walked round as W, W+D, S+D, S)
            if (!rb::parse_route("1 2.4 -8.2 8 - w:2,wd:1,sd:1,s:1", steps).empty() || steps.size() != 1 ||
                steps[0].how != rb::RouteStep::How::Timetable || steps[0].timetable.size() != 4 || steps[0].timetable[1].second != 1.0 ||
                rb::key_way(steps[0].timetable[2].first) != -135.0 || steps[0].keys != "w:2,wd:1,sd:1,s:1")
                return 89;
            for (const char* wrong_route : {"", " ; ", "13 1 2 5", "13 1 2 5 a b", "13 1 2 5 a ws", "13 1 2 5 a ad", "13 1 2 5 a WA", "13 1 2 5 a wa b",
                                            "13 1 2 5 a q", "13 1 2 5 a step d", "13 1 2 5 a tap tap", "13 1 2 5 a - -", "13 x 2 5 a", "13 1 2 -1 a", "13 40 2 5 a",
                                            "20 1 2 5 a; 13 1 2 5 a", "13 1 2 5 a w:2 tap", "13 1 2 5 a w:2,", "13 1 2 5 a w:0.05", "13 1 2 5 a q:1",
                                            "13 1 2 5 a w:11", "13 1 2 5 a w:x", "13 1 2 5 a w:10,s:10,w:10,s:1", "13 1 2 5 a w:2 w", "13 1 2 5 a w:2x"})
                if (rb::parse_route(wrong_route, steps).empty()) { std::cerr << "route accepted: \"" << wrong_route << "\"\n"; return 90; }
        }
        // Walking a route the way the game walks a person -- measured on beach-demo-003 C
        // (2026-09-28) and the earlier takes: a start goes 1.1 s straight where the camera
        // pointed as W went down (turned round first, and swung 2-7 degrees further, when the
        // body faced away), then the way follows the camera 0.3 s behind; let go, the person
        // slides on 0.52-1.51 m. Every spot is stood on near where it is (the slide's spread),
        // nobody closer than the route keeps them, W alone, no short steps, and no circling:
        // the old walker circled two spots for half a minute each in that take.
        // The second route is 003's C (2026-09-29, on W alone from 2026-10-09): a gentle bend after
        // the first spot, then short straight legs to and fro. Standing, the take turns the body where
        // the program means it to face (hold_view): to what it looks at, then to the next leg's way.
        struct RouteCase { const char* route; std::vector<size_t> spots; };
        const RouteCase route_cases[] = {
            // C's first route, then a leg bending 34 degrees round a way point (a person's own bend:
            // sharper ones swing out a metre, the turn being only as tight as the camera turns)
            {"12.5 -2.78 2.34 8 chat_a; 25 0.8 1.9 10 sitter; 40 1.3 0.3 0 -; 40 0.9 -1.0 6 chat_b", {0, 1, 3}},
            {"12.5 -2.78 2.34 3 chat_a; 12.5 -1.6 2.4 0 -; 12.5 -0.3 2.2 0 -; 12.5 0.9 1.7 8 sitter; 12.5 -1.3 2.2 8 chat_b; "
             "12.5 0.9 1.7 8 sitter", {0, 3, 4, 5}},
            // the same named keys: across on D, back on S (the camera turned to the way less the keys' way first)
            {"12.5 -2.78 2.34 3 chat_a d; 12.5 -1.6 2.4 0 -; 12.5 -0.3 2.2 0 -; 12.5 0.9 1.7 8 sitter wa; 12.5 -1.3 2.2 8 chat_b s; "
             "12.5 0.9 1.7 8 sitter sd", {0, 3, 4, 5}},
        };
        for (const RouteCase& rc : route_cases)
        for (const int fps : {30, 60})
        for (const double slide : {0.52, 0.93, 1.51}) {
            std::vector<rb::RouteStep> steps;
            rb::parse_route(rc.route, steps);
            const std::vector<rb::Person> crowd = {{"chat_a", -2.78, 1.38}, {"chat_b", -2.21, 0.64}, {"tourist", 2.74, 3.41}, {"sitter", 3.4, 0.2}};
            rb::RouteWalker walker(steps, 11.0);
            double x = 1.0, y = 2.6, body = 0, dir = 0, v = 0, cam = 0, pitch = -5, t = 0, gap = 1e9, since = 0, walked = 0;
            double walk_t = 0, lock = 0, stretch = 1, slide_v0 = 0, slide_t = 0, slide_T = 1, rate = 0;
            bool walking = false, fresh = false, sliding = false;
            double body_to = std::nan(""), turn_wait = 0;
            std::mt19937 hitch(7);
            rb::Keys held;
            int mdx = 0, mdy = 0, short_steps = 0;
            bool was = false, sideways = false, used_a = false, used_d = false, wrong_keys = false;
            std::vector<std::pair<double, double>> halts;
            auto start_speed = [](double s) {
                static const double T[][2] = {{0, 0}, {0.1, 0.5}, {0.2, 0.9}, {0.3, 1.1}, {0.4, 1.14}, {0.6, 1.04}, {0.8, 1.29}, {1.0, 1.8}, {1.2, 1.9}, {1.5, 1.72}};
                for (int i = 0; i + 1 < 10; ++i)
                    if (s <= T[i + 1][0]) return T[i][1] + (T[i + 1][1] - T[i][1]) * (s - T[i][0]) / (T[i + 1][0] - T[i][0]);
                return 1.72;
            };
            while (t < 60.0) {
                const double dt = hitch() % 50 == 0 ? 1.0 / 15 : 1.0 / fps;
                cam = rb::wrap(cam - 0.08 * mdx);
                pitch = std::clamp(pitch - 0.08 * mdy, -70.0, 45.0);
                const double before = dir;
                if (held.walks()) {
                    const double way = cam + rb::key_way(held);   // the keys' way off the camera: W+A 45 degrees left, S 180
                    if (!walking) {
                        walking = true; walk_t = 0; sliding = false; fresh = v < 0.3;
                        const double off = rb::wrap(body - way);
                        if (fresh) { lock = way + (std::fabs(off) >= 75.0 ? (off < 0 ? 4.5 : -4.5) : 0.0); dir = lock; }
                        stretch = 1.0 + 0.3 * std::clamp((std::fabs(off) - 90.0) / 90.0, 0.0, 1.0);
                    }
                    walk_t += dt;
                    v = fresh ? start_speed(walk_t / stretch) : std::min(1.72, v + 2.0 * dt);
                    if (fresh && walk_t < 1.1 * stretch) dir = lock;
                    else dir += rb::wrap(way - dir) * (1.0 - std::exp(-3.5 * dt));
                } else {
                    if (walking) {
                        walking = false; sliding = v > 0.05; slide_v0 = v; slide_t = 0;
                        slide_T = 2.0 * slide * std::min(1.0, v / 1.72) / std::max(v, 0.05);
                    }
                    if (sliding) { slide_t += dt; v = std::max(0.0, slide_v0 * (1.0 - slide_t / slide_T)); sliding = v > 0.0; }
                    else v = 0.0;
                    // standing, the take turns the body where the program means it to face (hold_view)
                    const double off = sliding || std::isnan(body_to) ? 0.0 : rb::wrap(body_to - body);
                    turn_wait = std::fabs(off) > 4.0 ? turn_wait + dt : 0.0;
                    if (turn_wait > kBodyTurnDelay) body = rb::wrap(body + body_turn(off, dt));
                }
                x += -std::sin(dir * kPi / 180.0) * v * dt;
                y += std::cos(dir * kPi / 180.0) * v * dt;
                if (sliding) { x += -std::cos(dir * kPi / 180.0) * 0.0022 * rate * v * dt / slide; y += -std::sin(dir * kPi / 180.0) * 0.0022 * rate * v * dt / slide; }
                else rate = rb::wrap(dir - before) / dt;
                if (v > 0.1) body = rb::wrap(body + std::clamp(rb::wrap(dir - body) * (1.0 - std::exp(-6.0 * dt)), -180.0 * dt, 180.0 * dt));
                t += dt;
                rb::WalkView view;
                view.t = t; view.dt = dt; view.x = x; view.y = y; view.heading = body; view.cam_yaw = cam; view.cam_pitch = pitch;
                view.people = crowd;
                const rb::Input in = walker.step(view);
                held = in.keys; mdx = in.mouse_dx; mdy = in.mouse_dy; body_to = in.body_to;
                sideways = sideways || held.a || held.d || held.s;
                used_a = used_a || held.a; used_d = used_d || held.d;
                wrong_keys = wrong_keys || held.q || held.e || (held.a && held.d) || (held.w && held.s);
                if (held.walks() && !was) since = t;
                if (held.walks()) walked += dt;
                if (!held.walks() && was && t - since < 0.4) ++short_steps;
                if (!held.walks() && was) halts.push_back({-1e9, -1e9});
                if (!halts.empty() && !held.walks() && v == 0.0 && halts.back().first < -1e8) halts.back() = {x, y};   // where the slide ended
                was = held.walks();
                for (const auto& q : crowd) gap = std::min(gap, std::hypot(x - q.x, y - q.y));
            }
            // every stop (a way point is walked round, not stopped at) within the slide's spread of
            // its spot -- the longest slide 0.58 m past the planned 0.93, and a walk on W+A or W+D a few
            // centimetres off to the side (0.65 m on 2026-10-16's keys) -- and the whole route in 11 s of walking:
            // a circle takes longer
            const std::vector<size_t>& spots = rc.spots;
            bool on_spots = halts.size() == spots.size() && walker.arrived() == spots.size() && walker.held_back() == 0;
            for (size_t i = 0; on_spots && i < spots.size(); ++i)
                on_spots = std::hypot(halts[i].first - steps[spots[i]].x, halts[i].second - steps[spots[i]].y) < 0.7;
            // the keys the route names for a leg are the keys it walks on (2026-10-16: any that walk)
            bool keys_ok = !wrong_keys && walker.legs_keys().size() >= spots.size();
            for (size_t i = 0; keys_ok && i < spots.size(); ++i) {
                rb::Keys named;
                if (rb::parse_keys(steps[spots[i]].keys, named)) keys_ok = walker.legs_keys()[i] == named;
            }
            // nobody nearer than the route keeps them (RUNBOOK: the chatting man 0.8 m, the tourist 0.72 m)
            if (!walker.mouse().ready() || !on_spots || !keys_ok || short_steps > 0 || gap < 0.8 || walked > 11.0) {
                std::cerr << "route \"" << rc.route << "\" at " << fps << " fps, slide " << slide << " m: spots " << walker.arrived() << " (stops " << halts.size() << ", held "
                          << walker.held_back() << "), keys A " << used_a << " D " << used_d << " wrong " << wrong_keys << ", short steps " << short_steps << ", closest " << gap
                          << " m, walked " << walked << " s\n";
                for (size_t i = 0; i < halts.size() && i < spots.size(); ++i)
                    std::cerr << "   stop " << i << " at " << halts[i].first << "," << halts[i].second << " for spot " << steps[spots[i]].x << "," << steps[spots[i]].y << "\n";
                return 91;
            }
        }
        // a key timetable: the camera set behind the body, then the keys on their timetable with the mouse untouched
        {
            std::vector<rb::RouteStep> steps;
            rb::TurnStyle walk;
            rb::parse_route("1 2.4 -8.2 8 - w:2,wd:1,sd:1,s:1", steps);
            rb::parse_turn("walk 40", walk);
            rb::RouteWalker walker(steps, 0.0, {}, walk);
            double x = 0, y = -9, body = 30, cam = 0, pitch = -5, t = 0, v = 0, pressed = -1, cam_at_press = 0;
            int mdx = 0, mdy = 0, mouse_moved = 0;
            std::vector<std::pair<double, rb::Keys>> changes;
            rb::Keys held;
            while (t < 20.0) {
                const double dt = 1.0 / 30;
                cam = rb::wrap(cam - 0.08 * mdx);
                pitch = std::clamp(pitch - 0.08 * mdy, -70.0, 45.0);
                if (held.walks()) { body = rb::wrap(cam + rb::key_way(held)); v = 1.7; }
                else v = std::max(0.0, v - 3.0 * dt);
                x += -std::sin(body * kPi / 180.0) * v * dt;
                y += std::cos(body * kPi / 180.0) * v * dt;
                t += dt;
                rb::WalkView view;
                view.t = t; view.dt = dt; view.x = x; view.y = y; view.heading = body; view.cam_yaw = cam; view.cam_pitch = pitch;
                const rb::Input in = walker.step(view);
                if (in.keys.walks() && pressed < 0) { pressed = t; cam_at_press = cam; }
                if (pressed >= 0 && t - pressed < 5.0 && (in.mouse_dx != 0 || in.mouse_dy != 0)) ++mouse_moved;
                if (in.keys.walks() && (changes.empty() || !(changes.back().second == in.keys))) changes.push_back({t, in.keys});
                held = in.keys; mdx = in.mouse_dx; mdy = in.mouse_dy;
            }
            // W 2 s, W+D 1 s, S+D 1 s, S 1 s, each on time; the camera behind the body (30 degrees) as W went down
            const double ways[] = {0.0, -45.0, -135.0, 180.0}, at[] = {0.0, 2.0, 3.0, 4.0};
            bool ok = pressed > 0 && changes.size() == 4 && mouse_moved == 0 && std::fabs(rb::wrap(cam_at_press - 30.0)) < 1.6 &&
                      walker.arrived() == 1 && walker.legs_keys().size() == 4;
            for (size_t i = 0; ok && i < 4; ++i)
                ok = std::fabs(rb::wrap(rb::key_way(changes[i].second) - ways[i])) < 1e-9 && std::fabs(changes[i].first - pressed - at[i]) < 0.05;
            if (!ok) {
                std::cerr << "timetable: pressed at " << pressed << ", " << changes.size() << " key changes, the mouse moved on " << mouse_moved
                          << " frames, the camera at " << cam_at_press << ", arrived " << walker.arrived() << "\n";
                return 124;
            }
        }
        // the map's own moving things the take logs (2026-10-16): track <id> <model or 0x hash> <x> <y> [radius]
        {
            const std::string head = "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 90\n";
            StageSpec ts;
            if (!parse_stage_spec(head + "track wheel prop_ld_ferris_wheel -40 60 80\ntrack flag_a 0x1234abcd 5.1 7.7\n", ts).empty() ||
                ts.tracks.size() != 2 || !ts.track("wheel") || ts.track("wheel")->radius != 80.0 || ts.track("flag_a")->radius != 5.0 ||
                ts.track("flag_a")->model != "0x1234abcd" || ts.track("flag_a")->x != 5.1)
                return 125;
            for (const char* wrong : {"track a\n", "track a m 1\n", "track a m x 1\n", "track a m 1 2 0.1\n", "track a m 1 2 300\n",
                                      "track a m 1 2 5 6\n", "track a m 1 2\ntrack a n 3 4\n"}) {
                StageSpec b;
                if (parse_stage_spec(head + wrong, b).empty()) { std::cerr << "track line accepted: " << wrong; return 126; }
            }
        }
        // the map's things a take removes (2026-10-18, the floats in the sea): clear <model or 0x hash> <x> <y> <radius>
        {
            const std::string head = "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 90\n";
            StageSpec cs;
            if (!parse_stage_spec(head + "clear prop_dock_float_1 0 0 200\nclear 0x73e498db 8.5 -3 1\nclear boats 0 0 200\n", cs).empty() || cs.clears.size() != 3 ||
                cs.clears[0].model != "prop_dock_float_1" || cs.clears[0].radius != 200.0 || cs.clears[1].model != "0x73e498db" ||
                cs.clears[1].x != 8.5 || cs.clears[1].y != -3.0)
                return 176;
            for (const char* wrong : {"clear m\n", "clear m 1 2\n", "clear m x 1 5\n", "clear m 1 2 0.5\n", "clear m 1 2 301\n", "clear m 1 2 5 6\n",
                                      "track a m 1 2\nclear m 0 0 50\n", "clear m 0 0 50\ntrack a m 1 2\n"}) {
                StageSpec b;
                if (parse_stage_spec(head + wrong, b).empty()) { std::cerr << "clear line accepted: " << wrong; return 177; }
            }
        }
        // how the played person stands (2026-10-18): stance game|calm|clip <loop clip>
        {
            const std::string head = "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 90\n";
            StageSpec st;
            if (!parse_stage_spec(head, st).empty() || st.stance != "game") return 180;
            if (!parse_stage_spec(head + "stance calm\n", st).empty() || st.stance != "calm") return 180;
            if (!parse_stage_spec(head + "stance clip amb@x@base base\n", st).empty() || st.stance != "clip" ||
                st.stance_dict != "amb@x@base" || st.stance_clip != "base")
                return 180;
            for (const char* wrong : {"stance\n", "stance still\n", "stance clip\n", "stance clip d\n", "stance clip d c e\n", "stance calm x\n"}) {
                StageSpec b;
                if (parse_stage_spec(head + wrong, b).empty()) { std::cerr << "stance line accepted: " << wrong; return 181; }
            }
        }
        // the cast's leg IK (2026-10-17): legik <0..3>, unset -1
        {
            const std::string head = "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 90\n";
            StageSpec ls;
            if (!parse_stage_spec(head, ls).empty() || ls.leg_ik != -1) return 127;
            if (!parse_stage_spec(head + "legik 2\n", ls).empty() || ls.leg_ik != 2 || ls.scene_ik_flags != 0) return 127;
            if (!parse_stage_spec(head + "legik 2 32\n", ls).empty() || ls.leg_ik != 2 || ls.scene_ik_flags != 32) return 127;
            for (const char* wrong : {"legik\n", "legik 4\n", "legik -1\n", "legik 1.5\n", "legik x\n", "legik 2 -1\n", "legik 2 70000\n",
                                      "legik 2 1 1\n"}) {
                StageSpec b;
                if (parse_stage_spec(head + wrong, b).empty()) { std::cerr << "legik line accepted: " << wrong; return 128; }
            }
        }
        // the spec names who plays: AB flies, C and D walk changed or unchanged, one line each
        const std::string head = "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 90\n";
        StageSpec rs;
        if (!parse_stage_spec(head + "robot AB fly 1\nrobot C changed 2\nrobot D unchanged 4294967295\n", rs).empty() || rs.robots.size() != 3 ||
            !rs.robot("D") || rs.robot("D")->seed != 4294967295u || rs.robot("C")->mode != "changed")
            return 71;
        for (const char* wrong : {"robot AB walk 1\n", "robot C fly 1\n", "robot B fly 1\n", "robot C changed -1\n", "robot C changed 1.5\n",
                                  "robot D unchanged\n", "robot C changed 1\nrobot C unchanged 2\n"}) {
            StageSpec b;
            if (parse_stage_spec(head + wrong, b).empty()) { std::cerr << "robot line accepted: " << wrong; return 72; }
        }
        // a mouse that turns nothing: the program stops rather than walk blind
        rb::Walker dead(9, false, 120.0, {});
        rb::WalkView v;
        v.y = -6;
        for (int f = 0; f < 240; ++f) { v.t = f / 60.0; dead.step(v); }
        if (!dead.mouse().failed()) { std::cerr << "a dead mouse was not noticed\n"; return 70; }
        // a mouse that turns the other way once calibrated (the wiggle was misread):
        // the steer turns the camera away -- following the body the take turns -- and the
        // program stops within seconds
        rb::Walker flipped(10, false, 120.0, {});
        double cam = 0, t = 0, body = 0, body_to = std::nan("");
        int mdx = 0;
        while (t < 30.0 && !flipped.mouse().failed()) {
            cam = rb::wrap(cam - (t < 4.0 ? 0.08 : -0.08) * mdx);
            if (!std::isnan(body_to)) body = rb::wrap(body + body_turn(rb::wrap(body_to - body), 1.0 / 60));
            t += 1.0 / 60;
            rb::WalkView w;
            w.t = t; w.dt = 1.0 / 60; w.y = -6; w.cam_yaw = cam; w.heading = body;
            const rb::Input in = flipped.step(w);
            mdx = in.mouse_dx;
            body_to = in.body_to;
        }
        if (!flipped.mouse().failed() || t > 20.0) { std::cerr << "a steer turning away was not noticed by " << t << " s\n"; return 73; }
    }

    // 15. the recorder's 30 fps cap (scripthook_game_adapter.cpp FramePacer): the
    // offset agrees with tick_of, and a game that waits for each deadline settles
    // one frame to a tick, the frames near the ticks' middles. A hitch costs one
    // tick and the frame after it catches up in the next one -- 30 frames a
    // second all the same, which is what the delivery's fps counts.
    {
        for (long long t = 900; t < 1200; ++t) {
            const double e = tick_offset_ms(t, 1000);
            // t - e is the tick's middle: 16 ms either side of it is the same tick
            if (e < -16.67 || e >= 16.67 || tick_of(static_cast<long long>(std::ceil(t - e - 16.0)), 1000) != tick_of(t, 1000) ||
                tick_of(static_cast<long long>(std::floor(t - e + 16.0)), 1000) != tick_of(t, 1000))
                return 75;
        }
        std::mt19937 rng(8);
        auto u = [&rng](double a, double b) { return robot::uniform(rng, a, b); };
        double now = 3.0, start = 0.0, game = 0.0, deadline = now, worst = 0.0;
        int last_tick = -1, doubled = 0, skipped = 0, hitches = 0, since_hitch = 100;
        for (int n = 0; n < 3600; ++n) {
            const long long g = std::llround(game);   // the game timer is whole milliseconds
            const double e = tick_offset_ms(g, 0);
            const int k = tick_of(g, 0);
            if (n >= 30) {   // settled within the recorder's first second, and a few frames after a hitch
                if (since_hitch >= 10) worst = std::max(worst, std::fabs(e));
                if (k == last_tick) ++doubled;
                if (k > last_tick + 1) skipped += k - last_tick - 1;
            }
            last_tick = k;
            deadline = next_frame_deadline(n == 0 ? now : deadline, now, e);
            const double wake = std::max(now, deadline);
            // the rest of the frame after the script, and a hitch now and then
            const bool hitch = n % 700 == 350;
            hitches += hitch;
            since_hitch = hitch ? 0 : since_hitch + 1;
            const double rest = (hitch ? 45.0 : 0.0) + u(1.0, 4.0);
            const double next = wake + rest;
            game += next - start;   // the game's step: the time between frame starts
            start = next;
            now = next + u(1.0, 4.0);   // the next frame's work before the script
        }
        if (doubled > hitches) { std::cerr << "frame cap: " << doubled << " ticks got two frames, " << hitches << " hitches\n"; return 76; }
        if (skipped > hitches) { std::cerr << "frame cap: " << skipped << " ticks skipped, " << hitches << " hitches\n"; return 77; }
        if (worst > 8.0) { std::cerr << "frame cap: a frame " << worst << " ms off its tick's middle\n"; return 78; }
    }

    // 16. set pieces, seats and the sink: a piece stands on the ground where the spec
    // puts it; a seated actor plays where the game seated a person on its piece, not
    // lowered; everybody else goes down by the sink; a seat the game did not show falls
    // back to "at" and says so; the calibration keeps a seat to the last bit.
    {
        const std::string head =
            "dm-gta-stage-spec/1\nframe -1566 -1144 1.5 30\nticks 90\nclip stand amb@a base loop\nclip sit amb@b base loop\n"
            "set chair prop_old_deck_chair|prop_chair_01a 2.0 1.0 90\nset car lguard -8 12 45\n"
            "actor s a_m_y_beach_01\nbeat s idle sit 3 at 2.0 1.0 90\nseat s chair sit\n"
            "actor t a_m_y_beach_02\nbeat t idle stand 3 at -1.0 0.0 0\n";
        StageSpec ss;
        if (!parse_stage_spec(head + "sink 0.02\n", ss).empty() || ss.sets.size() != 2 || ss.set("chair")->models.size() != 2 ||
            ss.set("chair")->models[1] != "prop_chair_01a" || !ss.seat("s") || ss.seat("s")->set != "chair" || ss.sink != 0.02)
            return 79;
        for (const char* wrong : {"sink 0.5\n", "sink -0.01\n", "set chair lguard 1 2 3\n", "set x | 1 2 3\n", "set y m 1 2\n",
                                  "seat s chair sit\n", "seat t nothing stand\n", "seat nobody chair sit\n"}) {
            StageSpec b;
            if (parse_stage_spec(head + wrong, b).empty()) { std::cerr << "set/seat/sink line accepted: " << wrong; return 80; }
        }
        Calibration sc = synthetic_calibration(ss);
        sc.seats.push_back({"s", true, 0.1, -0.3, 0.45, 180.0});
        Calibration back;
        if (!parse_calibration(render_calibration(sc), back).empty() || render_calibration(back) != render_calibration(sc) ||
            !back.seat("s") || !back.seat("s")->found || back.seat("s")->z != 0.45)
            return 81;
        auto flat = [](double, double) { return 1.5; };
        Plan sp;
        if (!resolve_plan(ss, sc, flat, sp).empty() || sp.sets.size() != 2) return 82;
        const SetPlacement& chair = sp.sets[0];
        double cx = 0, cy = 0;
        ss.frame.to_world(2.0, 1.0, cx, cy);
        if (std::fabs(chair.x - cx) > 1e-9 || std::fabs(chair.y - cy) > 1e-9 || chair.z != 1.5 || chair.heading != 120.0) return 83;
        const ResolvedBeat& sat = sp.actors[0].beats[0];
        const double c = std::cos(chair.heading * kPi / 180.0), s = std::sin(chair.heading * kPi / 180.0);
        if (std::fabs(sat.ox - (chair.x + 0.1 * c + 0.3 * s)) > 1e-9 || std::fabs(sat.oy - (chair.y + 0.1 * s - 0.3 * c)) > 1e-9 ||
            std::fabs(sat.oz - 1.95) > 1e-9 || std::fabs(sat.heading - 300.0) > 1e-9)
            { std::cerr << "the seated actor is not where the game seated a person\n"; return 84; }
        StageSpec unsunk;
        Plan up;
        if (!parse_stage_spec(head, unsunk).empty() || !resolve_plan(unsunk, sc, flat, up).empty()) return 85;
        if (std::fabs(up.actors[1].beats[0].oz - sp.actors[1].beats[0].oz - 0.02) > 1e-12 || up.actors[0].beats[0].oz != sat.oz)
            { std::cerr << "the sink lowered the wrong actors\n"; return 86; }
        sc.seats[0].found = false;   // the game seated nobody: the actor stands where "at" puts it, and the plan says so
        Plan fp;
        if (!resolve_plan(ss, sc, flat, fp).empty() || fp.actors[0].beats[0].ox == sat.ox) return 87;
        bool said = false;
        for (const auto& n : fp.notes) said = said || n.find("seated nobody") != std::string::npos;
        if (!said) return 88;
    }
    // A clip lifted (lift): that clip's scenes that much higher, standing and walking; nobody else's,
    // and not a seated one
    {
        const std::string head =
            "dm-gta-stage-spec/1\nframe -1566 -1144 1.5 30\nticks 300\nsink 0.02\nclip stand amb@a base loop\nclip sit amb@b base loop\n"
            "clip walk move_m@generic walk walk\npath p 0 0 0 0\nline p 4.0\n"
            "set chair prop_chair_01a 2.0 1.0 90\n"
            "actor s a_m_y_beach_01\nbeat s idle sit 3 at 2.0 1.0 90\nseat s chair sit\n"
            "actor t a_m_y_beach_02\nbeat t idle stand 3 at -1.0 0.0 0\n"
            "actor w a_m_y_beach_03\nbeat w walk walk p\n";
        StageSpec plain, lifted;
        if (!parse_stage_spec(head, plain).empty() ||
            !parse_stage_spec(head + "lift stand 0.03\nlift walk -0.01\nlift sit 0.05\n", lifted).empty() ||
            lifted.lifts.size() != 3 || lifted.lift("stand") != 0.03 || lifted.lift("walk") != -0.01 || plain.lift("stand") != 0.0)
            return 94;
        for (const char* wrong : {"lift stand\n", "lift stand 0.2\n", "lift stand -0.11\n", "lift nothing 0.01\n", "lift stand x\n",
                                  "lift stand 0.01\nlift stand 0.02\n"}) {
            StageSpec b;
            if (parse_stage_spec(head + wrong, b).empty()) { std::cerr << "lift line accepted: " << wrong; return 95; }
        }
        Calibration lc = synthetic_calibration(plain);
        lc.seats.push_back({"s", true, 0.1, -0.3, 0.45, 180.0});
        auto flat = [](double, double) { return 1.5; };
        Plan p0, p1;
        if (!resolve_plan(plain, lc, flat, p0).empty() || !resolve_plan(lifted, lc, flat, p1).empty()) return 96;
        const double stood = p1.actors[1].beats[0].oz - p0.actors[1].beats[0].oz;
        const double sat = p1.actors[0].beats[0].oz - p0.actors[0].beats[0].oz;
        const double walked = tick_op(p1, lc, 2, 10).oz - tick_op(p0, lc, 2, 10).oz;
        if (std::fabs(stood - 0.03) > 1e-12 || sat != 0.0 || std::fabs(walked + 0.01) > 1e-12 || !tick_op(p1, lc, 2, 10).walking ||
            render_plan(p1).find("lift stand 0.03") == std::string::npos || render_plan(p0).find("lift") != std::string::npos) {
            std::cerr << "lift: stood " << stood << " sat " << sat << " walked " << walked << "\n";
            return 97;
        }
    }
    // When hand props show (propshow): only in the named clips between the phases given
    {
        const std::string head =
            "dm-gta-stage-spec/1\nframe 0 0 0 0\nticks 90\nclip call amb@a base loop\nclip away amb@b exit once\n"
            "actor p a_m_y_hipster_01\nprop p prop_npc_phone_02 28422\nbeat p idle call 3 at 0 0 0\n"
            "actor q a_f_y_tourist_01\nprop q prop_pap_camera_01 28422\nbeat q idle call 3 at 2 0 0\n";
        StageSpec ps;
        if (!parse_stage_spec(head + "propshow p call 0 1\npropshow p away 0 0.72\n", ps).empty() || ps.actors[0].prop_shows.size() != 2 ||
            ps.actors[0].prop_shows[1].clip != "away" || ps.actors[0].prop_shows[1].to != 0.72 || !ps.actors[1].prop_shows.empty())
            return 92;
        for (const char* wrong : {"propshow p call\n", "propshow p call 0.5 0.5\n", "propshow p call -0.1 1\n", "propshow p call 0 1.2\n",
                                  "propshow p nothing 0 1\n", "propshow nobody call 0 1\n", "propshow p call x 1\n"}) {
            StageSpec b;
            if (parse_stage_spec(head + wrong, b).empty()) { std::cerr << "propshow line accepted: " << wrong; return 93; }
        }
    }

    int notes = static_cast<int>(plan.notes.size());
    std::printf("stage plan contract passed: %zu actors, %d beats, worst snap jump %.3f m, %d stops, %d planner notes%s\n",
                plan.actors.size(),
                static_cast<int>(plan.actors[0].beats.size() + plan.actors[1].beats.size() + plan.actors[2].beats.size()),
                worst_jump, stops, notes, argc > 1 ? ", demo stage resolved" : "");
    return 0;
}
