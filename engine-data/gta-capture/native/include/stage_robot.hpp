#pragma once

// Program input for the four-worldline takes (2026-09-24). Worldline B's free
// camera and the person played in C and D are driven by a program instead of
// a person: it presses keys and moves the mouse through the path a person's
// keyboard and mouse take (SendInput, stage_take_win.cpp), so a take still
// reads -- never writes -- its input log, and every key logged is a key that
// moved the camera or the person. What to press is decided here, from what the
// program sees each frame; nothing here touches the game, so the stage selftest
// runs it against a model of the free camera and of a walking person.
//
//   fly        (AB) legs of 1.5-3.5 s, each holding a key combination for the free
//              camera -- W A S D Q E all among them -- or hovering, while the mouse
//              keeps the cast's middle in frame, now and then turning away and
//              back. A guard takes over whenever the camera heads for a person,
//              the flags behind the stage, the ground, or out of the stage's reach.
//   changed    (C/D) walks up to members of the cast one after another -- what
//              they do about it is the changed event -- then strolls in front of
//              the stage.
//   unchanged  (C/D) strolls across the front of the stage and stops to watch,
//              never within reach of anybody's rule.
//   route      (C) a fixed route instead of dice (the user, 2026-09-28): spot after
//              spot, none before its time, standing at each looking at whom it names;
//              checked against the recorded cast before anybody records.
// The person is played as a player plays GTA's third person (the customer, 2026-10-16): the
// mouse turns the camera, which may come round to the person's side or face; W, A, S and D
// all walk -- in the third person they do not step sideways or back, the person turns to that
// way off the camera and walks it (W+A 45 degrees left of the camera, A 90, S+A 135, S 180 ...,
// key_way); and the first person looks where the body faces (the head goes with it). So the
// body turns first, standing, at the glide's pace (TurnStyle), to where it will walk, and the
// keys that walk it there off the camera go down; walking, only the mouse steers. Standing, the
// body turns to what it looks at and the camera comes round after it.
//
// Degrees per mouse count are the game's setting, not known here: control
// starts with a wiggle (right and back, down and back) whose measured effect
// sets the scale; a wiggle that turns nothing means the input does not reach
// the game, and the program stops.
//
// Stage coordinates throughout (x, y; heights above the ground); headings are
// GTA's: 0 faces +y and a heading h faces (-sin h, cos h), so a left turn adds.

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <cstdlib>
#include <random>
#include <sstream>
#include <string>
#include <utility>
#include <vector>

namespace diogenes::gta::stage::robot {

constexpr double kPi = 3.14159265358979323846;

inline double wrap(double a) { return std::fmod(std::fmod(a + 180.0, 360.0) + 360.0, 360.0) - 180.0; }
inline double heading_of(double dx, double dy) { return std::atan2(-dx, dy) * 180.0 / kPi; }
inline double smooth01(double t) { t = std::clamp(t, 0.0, 1.0); return t * t * t * (10.0 + t * (-15.0 + 6.0 * t)); }
inline double bump01(double t) { t = std::clamp(t, 0.0, 1.0); return 64.0 * std::pow(t * (1.0 - t), 3.0); }

struct Keys {
    bool w = false, a = false, s = false, d = false, q = false, e = false;
    bool operator==(const Keys& o) const { return w == o.w && a == o.a && s == o.s && d == o.d && q == o.q && e == o.e; }
    bool any() const { return w || a || s || d || q || e; }
    bool walks() const { return w || a || s || d; }
};
// Where a key combination walks a person in GTA's third person, degrees off the camera (a left turn adds):
// W 0, W+A 45, A 90, S+A 135, S 180, S+D -135, D -90, W+D -45 (W+A / W+D / A / D measured on 003's strolls).
inline double key_way(const Keys& k) {
    const double f = (k.w ? 1.0 : 0.0) - (k.s ? 1.0 : 0.0), l = (k.a ? 1.0 : 0.0) - (k.d ? 1.0 : 0.0);
    return std::atan2(l, f) * 180.0 / 3.14159265358979323846;
}
// The combination that walks nearest `off` degrees off the camera.
inline Keys keys_for(double off) {
    off = std::fmod(std::fmod(off + 180.0, 360.0) + 360.0, 360.0) - 180.0;
    const int n = static_cast<int>(std::lround(off / 45.0));   // -4 .. 4
    Keys k;
    k.w = n >= -1 && n <= 1;
    k.s = n <= -3 || n >= 3;
    k.a = n >= 1 && n <= 3;
    k.d = n <= -1 && n >= -3;
    return k;
}
// The single key -- W, A, S or D -- that walks nearest `off` degrees off the camera. A tap goes on one key: on two
// (W+D, S+A ...) the game walked a metre 16 times in 41, on one it turned on the spot 16 times in 16 (win-pc,
// 2026-10-17: beach-demo-101's C and D, 102's C, the wind probe's D).
inline Keys key_for(double off) {
    off = std::fmod(std::fmod(off + 180.0, 360.0) + 360.0, 360.0) - 180.0;
    const int n = static_cast<int>(std::lround(off / 90.0));   // -2 .. 2
    Keys k;
    k.w = n == 0;
    k.a = n == 1;
    k.s = n == 2 || n == -2;
    k.d = n == -1;
    return k;
}
// "w", "a", "s", "d", "wa", "wd", "sa" or "sd" into keys; false for anything else.
inline bool parse_keys(const std::string& text, Keys& k) {
    static const char* const known[] = {"w", "a", "s", "d", "wa", "wd", "sa", "sd"};
    k = Keys{};
    for (const char* name : known)
        if (text == name) {
            for (const char c : text) { k.w |= c == 'w'; k.a |= c == 'a'; k.s |= c == 's'; k.d |= c == 'd'; }
            return true;
        }
    return false;
}
// "w:2,wd:1,sd:1,s:1" into a key timetable: each combination held that many seconds (0.1-10), in turn, 30 s in all
// at most -- the customer's way round a big turn (2026-10-16: W, then W+D, then S+D, then S, the path an arc); false
// for anything else.
inline bool parse_timetable(const std::string& text, std::vector<std::pair<Keys, double>>& out) {
    out.clear();
    if (text.find(':') == std::string::npos) return false;
    std::stringstream all(text);
    double total = 0.0;
    for (std::string item; std::getline(all, item, ',');) {
        const size_t colon = item.find(':');
        Keys k;
        if (colon == std::string::npos || !parse_keys(item.substr(0, colon), k)) return false;
        const std::string number = item.substr(colon + 1);
        char* end = nullptr;
        const double held = std::strtod(number.c_str(), &end);
        if (number.empty() || end != number.c_str() + number.size() || !std::isfinite(held) || held < 0.1 || held > 10.0) return false;
        out.push_back({k, held});
        total += held;
    }
    return !out.empty() && total <= 30.0 && text.back() != ',';
}
// The camera's pitch the walkers keep: a little down -- the third person is to look level or a little down. (Till
// 2026-10-18 the first person's pitch was the camera's, and 12 degrees down kept a person 2-3 m off in it to the hips;
// since, the first person is level: stage_tape.FP_PITCH.)
constexpr double kLookPitchDeg = -12.0;
// mouse in counts, this frame; body_to: where the standing body is to turn (stage-local heading),
// turned by the take (stage_take hold_view) -- the game's third person turns it only once a key goes down
struct Input { Keys keys; int mouse_dx = 0, mouse_dy = 0; double body_to = std::nan(""); };
// The body turns first (the customer, 2026-10-09: the first-person view goes with the face): a walk
// sets off once the body faces its way, and the camera that way less the keys' way, within kAlignDeg.
constexpr double kAlignDeg = 8.0;
// ... and once the body has stopped turning: the game's turn on the spot swings on past where it
// was asked to stop (60 degrees once), and a walk set off in mid-swing went round in circles.
constexpr double kSettledDegS = 15.0;

struct Person { std::string id; double x = 0, y = 0; };

// How a standing body turns (the customer, 2026-10-10: turning on the spot is too fast -- route it
// so there is no turn, or turn slower and through smaller angles). The game turns a person on the
// spot in half a second to a second whatever the angle -- 90 degrees at 130-190 deg/s, 180 at
// 170-220 -- and the first-person view goes with the face, so the view was over the requirement's
// 90 deg/s (a p95 limit) for 8 s of C's 120 and 11 s of D's (beach-demo-003, 2026-10-09).
//   game    one go to where the body is to face (TASK_ACHIEVE_HEADING): what 003 recorded
//   steps   the game's own turn, kTurnStepDeg at a time (about 85 deg/s at the start of a 30-degree
//           turn, never over 90), each step once the last has stopped: the feet step as the game's
//   glide   the take sets the heading itself every frame (stage_take hold_view): up to `rate` deg/s,
//           eased in and out at `accel` deg/s^2 -- the view turns as the requirement's limits allow
//           (90 deg/s, 150 deg/s^2), but the feet do not step: look at it before recording with it
//   walk    the program never turns the body (the customer, 2026-10-16: played as a player plays GTA;
//           the user: the game's own turns): the keys turn it as they go down -- a route turns it
//           standing with a tap (RouteStep) -- and the camera turns at `rate` deg/s at most. The game
//           turns it fast (90 degrees at ~180 deg/s); the first-person lane comes round after it slower
//           (stage_tape FP_MAX_RATE: the customer, the view eased, slower than the body).
// A turn of more than kSweepDeg goes round the side that keeps more of the cast in view, not the shorter.
enum class TurnMode { Game, Steps, Glide, Walk };
// walk ... straight: a leg sets off the way the body faces whenever its way is within kStraightStartDeg of it, and the
// camera brings the walk round onto the leg -- no turn on the spot at a start (the user, 2026-10-17: the played
// people's turns on the spot read as their feet sliding; turned only while walking, as the cast now is).
struct TurnStyle { TurnMode mode = TurnMode::Game; double rate = 45.0, accel = 90.0; bool straight = false; };
constexpr double kStraightStartDeg = 45.0;
constexpr double kTurnStepDeg = 30.0, kSweepDeg = 100.0, kStepSettledDegS = 10.0;
// Half the first-person view's width (stage_tape.FP_FOCAL_MM: 24 mm on a 36 mm film back, 74 degrees wide; 18 mm and
// 90 degrees till 2026-10-16, when the customer found it too wide).
constexpr double kFirstPersonHalfDeg = 36.9;

// "game", "steps", "glide <deg/s> <deg/s^2>" (or "glide": 45, 90), "walk [<deg/s>] [straight]" (60) into a style; "" or
// what is wrong.
inline std::string parse_turn(const std::string& text, TurnStyle& out) {
    out = TurnStyle{};
    std::istringstream in(text);
    std::string mode, extra;
    if (!(in >> mode)) return "";   // nothing said: the game's own turn
    if (mode == "game" || mode == "steps") {
        out.mode = mode == "game" ? TurnMode::Game : TurnMode::Steps;
        return (in >> extra) ? "turn \"" + text + "\": " + mode + " takes no numbers" : "";
    }
    if (mode == "walk") {
        out.mode = TurnMode::Walk;
        out.rate = 60.0;
        std::string word;
        bool rated = false;
        while (in >> word) {
            if (word == "straight" && !out.straight) { out.straight = true; continue; }
            std::istringstream num(word);
            double rate = 0;
            if (rated || out.straight || !(num >> rate) || (num >> extra))
                return "turn \"" + text + "\": walk takes the camera's deg/s at most, then maybe straight";
            out.rate = rate;
            rated = true;
        }
        if (!std::isfinite(out.rate) || out.rate < 10.0 || out.rate > 90.0) return "turn \"" + text + "\": walk turns the camera at 10-90 deg/s";
        return "";
    }
    if (mode != "glide") return "turn \"" + text + "\": game, steps, glide <deg/s> <deg/s^2> or walk [<deg/s>]";
    out.mode = TurnMode::Glide;
    double rate = 0, accel = 0;
    if (in >> rate) {
        if (!(in >> accel) || (in >> extra)) return "turn \"" + text + "\": glide takes a rate and an acceleration";
        out.rate = rate; out.accel = accel;
    }
    if (!std::isfinite(out.rate) || !std::isfinite(out.accel) || out.rate < 10.0 || out.rate > 90.0 || out.accel < 20.0 || out.accel > 150.0)
        return "turn \"" + text + "\": a glide turns at 10-90 deg/s, eased at 20-150 deg/s^2 (the requirement's limits)";
    return "";
}
inline std::string turn_text(const TurnStyle& s) {
    if (s.mode == TurnMode::Game) return "game";
    if (s.mode == TurnMode::Steps) return "steps";
    std::ostringstream o;
    if (s.mode == TurnMode::Walk) { o << "walk " << s.rate << (s.straight ? " straight" : ""); return o.str(); }
    o << "glide " << s.rate << " " << s.accel;
    return o.str();
}

class Turner {
public:
    explicit Turner(TurnStyle style = {}) : style_(style) {}
    // Where the body is to face now, on its way to `want`; the body faces `heading`, turning at `rate` deg/s.
    double to(double t, double dt, double heading, double rate, double want, const std::vector<Person>& people, double x, double y) {
        if (style_.mode == TurnMode::Walk) return std::nan("");   // only the keys turn it
        if (style_.mode == TurnMode::Game) return want;
        return style_.mode == TurnMode::Steps ? step_to(t, heading, rate, want, people, x, y) : glide_to(dt, heading, want, people, x, y);
    }
    int turns() const { return turns_; }
    // walking: the keys turn the body; a glide starts again from where it then stands
    void reset() { stepping_ = false; gliding_ = false; side_ = 0; glide_rate_ = 0.0; }
    const TurnStyle& style() const { return style_; }

private:
    double step_to(double t, double heading, double rate, double want, const std::vector<Person>& people, double x, double y) {
        const double off = wrap(want - heading);
        if (stepping_) {   // a step is on: until the body stands at its end
            if (std::fabs(wrap(step_to_ - heading)) < 4.0 && std::fabs(rate) < kStepSettledDegS) stepping_ = false;
            else return step_to_;
        }
        if (std::fabs(off) <= kTurnStepDeg + 1.0) { side_ = 0; return want; }
        (void)t;
        if (side_ == 0) { side_ = way_round(heading, off, people, x, y); ++turns_; }
        const double step = std::clamp(left(off), -kTurnStepDeg, kTurnStepDeg);
        step_to_ = wrap(heading + step);
        stepping_ = true;
        return step_to_;
    }
    double glide_to(double dt, double heading, double want, const std::vector<Person>& people, double x, double y) {
        if (!gliding_) { carrot_ = heading; gliding_ = true; glide_rate_ = 0.0; side_ = 0; }
        const double off = wrap(want - carrot_);
        if (std::fabs(off) < 0.05 && std::fabs(glide_rate_) < 1.0) { glide_rate_ = 0.0; side_ = 0; carrot_ = want; return carrot_; }
        if (side_ == 0) { side_ = way_round(carrot_, off, people, x, y); if (std::fabs(off) > 4.0) ++turns_; }
        const double rest = left(off);   // the way still to go, round the side chosen
        // as fast as the rate allows and still stop on it at the easing, a frame at a time: moving at v
        // this frame and easing off after it covers v dt / 2 + v^2 / 2a (the continuous v = sqrt(2 a rest)
        // came in too fast and stopped dead in the last frame: 280 deg/s^2 for a 100 limit)
        const double a = style_.accel, h = a * dt / 2.0;
        const double brake = -h + std::sqrt(h * h + 2.0 * a * std::fabs(rest));
        const double want_rate = (rest < 0 ? -1.0 : 1.0) * std::min(style_.rate, brake);
        glide_rate_ += std::clamp(want_rate - glide_rate_, -a * dt, a * dt);
        double step = glide_rate_ * dt;
        if ((std::fabs(step) >= std::fabs(rest) && step * rest >= 0) || std::fabs(rest) < 0.5 * a * dt * dt) {
            step = rest; glide_rate_ = 0.0; side_ = 0;
        }
        carrot_ = wrap(carrot_ + step);
        return carrot_;
    }
    // the turn still to go round the side chosen
    double left(double off) const {
        if (side_ > 0) return off >= 0 ? off : off + 360.0;
        if (side_ < 0) return off <= 0 ? off : off - 360.0;
        return off;
    }
    // +1 (left, the heading growing) or -1: the shorter way, or past kSweepDeg the way that looks at
    // nobody for less of it, 10 degrees at a time (counting what is seen, the longer way round always won:
    // 252 degrees instead of 108, half of it at nobody -- route_sim, 2026-10-13)
    static int way_round(double heading, double off, const std::vector<Person>& people, double x, double y) {
        if (std::fabs(off) <= kSweepDeg) return off >= 0 ? 1 : -1;
        auto blind = [&](int side) {
            const double span = side > 0 ? (off >= 0 ? off : off + 360.0) : (off <= 0 ? -off : 360.0 - off);
            int n = 0;
            for (double a = 5.0; a < span; a += 10.0) {
                bool seen = false;
                for (const Person& p : people) seen = seen || std::fabs(wrap(heading_of(p.x - x, p.y - y) - (heading + side * a))) < kFirstPersonHalfDeg - 5.0;
                n += !seen;
            }
            return std::pair<int, double>(n, span);   // less at nobody, then the shorter way
        };
        return blind(1) <= blind(-1) ? 1 : -1;
    }
    TurnStyle style_;
    bool stepping_ = false, gliding_ = false;
    double step_to_ = 0, carrot_ = 0, glide_rate_ = 0;
    int side_ = 0, turns_ = 0;
};

// The mouse: a calibrating wiggle, then a steer toward a wanted yaw and pitch.
// Counts go out whole; what is left of a count carries to the next frame.
class Mouse {
public:
    // Counts for this frame. `yaw`/`pitch`: the camera now; want_*: where it should look. Following a
    // turning body: `ff` its turn rate (deg/s, fed forward), with a harder `gain` and `accel` (Follow).
    void step(double t, double dt, double yaw, double pitch, double want_yaw, double want_pitch, double max_rate,
              int& dx, int& dy, double ff = 0.0, double gain = 2.2, double accel = kTurnAccel) {
        dx = dy = 0;
        if (phase_ == Phase::Done || phase_ == Phase::Failed) {
            if (phase_ == Phase::Failed) return;
            // A scale calibrated with the wrong sign steers away: the error then settles
            // near 180 degrees and stays. Turned away that long is never steering (a
            // half turn at the slowest rate takes under 5 s), so the program stops.
            const double error = wrap(want_yaw - yaw);
            away_s_ = std::fabs(error) > kAwayDeg ? away_s_ + dt : 0.0;
            if (away_s_ > kAwayS) { phase_ = Phase::Failed; return; }
            // gain 2.2/s: the error closes with a ~0.5 s time constant
            const double ry = std::clamp(ff + gain * error, -max_rate, max_rate);
            const double rp = std::clamp(gain * (want_pitch - pitch), -0.6 * max_rate, 0.6 * max_rate);
            // The turn eases in and out, as a hand turns: a turn that jumps to speed in
            // one frame is a jolt in the chase and first-person views (beach-demo-002 C).
            // The ease keeps up with the steer (gain x max rate is under it), so it adds no overshoot.
            const double ease = accel * dt;
            rate_y_ += std::clamp(ry - rate_y_, -ease, ease);
            rate_p_ += std::clamp(rp - rate_p_, -ease, ease);
            carry_x_ += rate_y_ * dt / k_yaw_;
            carry_y_ += rate_p_ * dt / k_pitch_;
            const double most = std::max(40.0, 1.5 * max_rate * dt / std::fabs(k_yaw_));   // counts this frame at most
            dx = static_cast<int>(std::clamp(std::trunc(carry_x_), -most, most));
            dy = static_cast<int>(std::clamp(std::trunc(carry_y_), -40.0, 40.0));
            carry_x_ -= dx; carry_y_ -= dy;
            return;
        }
        calibrate(t, yaw, pitch, dx, dy);
    }
    bool ready() const { return phase_ == Phase::Done; }
    bool failed() const { return phase_ == Phase::Failed; }
    void still() { rate_y_ = rate_p_ = carry_x_ = carry_y_ = 0.0; }   // let go of the mouse: the next turn eases in from rest
    double k_yaw() const { return k_yaw_; }
    double k_pitch() const { return k_pitch_; }

private:
    enum class Phase { Start, Out, Settle, Back, Rest, Done, Failed };
    static constexpr double kAwayDeg = 100.0, kAwayS = 8.0;   // turned away this far this long: the steer is broken
    static constexpr double kTurnAccel = 240.0;   // deg/s^2 the turn rate changes by at most: a turn up to speed in a sixth of a second
    static constexpr int kCounts = 2;       // per frame while wiggling
    static constexpr double kOut = 0.35;    // s of wiggle each way
    static constexpr double kSettle = 0.45; // s for the camera to come to rest
    // Both axes at once -- right and down, then back -- about 1.6 s in all.
    void calibrate(double t, double yaw, double pitch, int& dx, int& dy) {
        switch (phase_) {
        case Phase::Start: t0_ = t; yaw0_ = yaw; pitch0_ = pitch; sent_ = 0; phase_ = Phase::Out; [[fallthrough]];
        case Phase::Out:
            if (t - t0_ < kOut) { dx = dy = kCounts; sent_ += kCounts; return; }
            t0_ = t; phase_ = Phase::Settle; return;
        case Phase::Settle:
            if (t - t0_ < kSettle) return;
            k_yaw_ = sent_ ? wrap(yaw - yaw0_) / sent_ : 0.0;
            if (std::fabs(k_yaw_) < 1e-3) { phase_ = Phase::Failed; return; }
            k_pitch_ = sent_ ? (pitch - pitch0_) / sent_ : 0.0;
            if (std::fabs(k_pitch_) < 1e-3) k_pitch_ = -std::fabs(k_yaw_);   // a clamped pitch says nothing: mouse down looks down
            back_ = sent_; sent_ = 0; t0_ = t; phase_ = Phase::Back; [[fallthrough]];
        case Phase::Back:
            if (sent_ < back_) { dx = dy = -std::min(kCounts, back_ - sent_); sent_ -= dx; return; }
            t0_ = t; phase_ = Phase::Rest; return;
        case Phase::Rest:
            if (t - t0_ < kSettle) return;
            phase_ = Phase::Done; return;
        default: return;
        }
    }
    Phase phase_ = Phase::Start;
    double t0_ = 0, yaw0_ = 0, pitch0_ = 0;
    int sent_ = 0, back_ = 0;
    double k_yaw_ = 0, k_pitch_ = 0;         // degrees per count, signed
    double carry_x_ = 0, carry_y_ = 0;
    double rate_y_ = 0, rate_p_ = 0;         // the turn now, deg/s: eased toward what the steer asks
    double away_s_ = 0;                      // how long the camera has been turned far from where it should look
};

// ------------------------------------------------------------------ fly (B)

struct FlyView {
    double t = 0, dt = 1.0 / 60;        // s since tick 0; this frame's step
    double x = 0, y = 0, z = 0;         // the free camera; z above the ground
    double yaw = 0, pitch = 0;
    double fx = 0, fy = 0, fz = 1.2;    // the cast's middle, to keep in frame
    std::vector<Person> people;
};

struct FlyRoom {   // where the free camera may go
    double max_y = 2.5;         // the flags stand behind the stage (the camera coasts ~1 m past a line)
    double cx = 0, cy = 1.6, reach = 10.5;
    double min_z = 1.5, max_z = 5.5;
    double clearance = 2.2;     // from anybody: a person is a post up to kPersonTop
    double speed = 2.0, climb = 1.0;   // what a held key flies at (stage_take_win.cpp fly())
};
constexpr double kPersonTop = 1.9;   // m above the ground

// Draws that come out the same on every compiler. std::mt19937's raw output is fixed
// by the standard, but uniform_real_distribution and shuffle are not: libstdc++,
// libc++ and MSVC turn one seed into three different walks, and a seed checked on
// Windows walked 28.8% of a take under Linux CI (the self-test wants 30%).
inline double uniform(std::mt19937& rng, double a, double b) {
    return a + (b - a) * (static_cast<double>(rng()) / 4294967296.0);
}
template <class It>
void shuffle(It first, It last, std::mt19937& rng) {   // Fisher-Yates on the raw draws
    for (auto n = last - first; n > 1; --n) {
        const auto pick = static_cast<decltype(n)>(static_cast<std::uint32_t>(rng()) % static_cast<std::uint32_t>(n));
        std::iter_swap(first + (n - 1), first + pick);
    }
}

// How far a camera at (x, y, z) is from a person standing at (px, py): a person is
// a post from the ground to kPersonTop.
inline double gap_to(double x, double y, double z, double px, double py) {
    return std::hypot(std::hypot(x - px, y - py), std::max(0.0, z - kPersonTop));
}

class Flyer {
public:
    Flyer(std::uint32_t seed, double duration_s, FlyRoom room = {}) : room_(room) {
        std::mt19937 rng(seed);
        auto uni = [&](double a, double b) { return uniform(rng, a, b); };
        // every key twice at least, then random combinations; hovers between
        const Keys W{1, 0, 0, 0, 0, 0}, A{0, 1, 0, 0, 0, 0}, S{0, 0, 1, 0, 0, 0}, D{0, 0, 0, 1, 0, 0}, Q{0, 0, 0, 0, 1, 0}, E{0, 0, 0, 0, 0, 1};
        const Keys WA{1, 1, 0, 0, 0, 0}, WD{1, 0, 0, 1, 0, 0}, SA{0, 1, 1, 0, 0, 0}, SD{0, 0, 1, 1, 0, 0}, WE{1, 0, 0, 0, 0, 1},
            SQ{0, 0, 1, 0, 1, 0}, AE{0, 1, 0, 0, 0, 1}, DQ{0, 0, 0, 1, 1, 0};
        std::vector<Keys> deck = {W, A, S, D, Q, E, W, A, S, D, Q, E};
        const std::vector<Keys> extra = {WA, WD, SA, SD, WE, SQ, AE, DQ, W, S, A, D};
        robot::shuffle(deck.begin(), deck.end(), rng);
        double t = 0;
        size_t i = 0;
        while (t < duration_s) {
            const Keys k = i < deck.size() ? deck[i] : extra[rng() % extra.size()];
            ++i;
            Leg move{t, t + uni(1.5, 3.5), k, 0.0};
            if (uni(0, 1) < 0.2) move.pan = uni(22, 32) * (rng() % 2 ? 1 : -1);
            legs_.push_back(move);
            t = move.t1;
            if (uni(0, 1) < 0.5) {   // a hover: look, maybe turn away and back
                Leg hover{t, t + uni(1.0, 3.0), Keys{}, uni(0, 1) < 0.4 ? uni(22, 32) * (rng() % 2 ? 1 : -1) : 0.0};
                legs_.push_back(hover);
                t = hover.t1;
            }
        }
    }

    Input step(const FlyView& v) {
        Input in;
        if (have_last_ && v.dt > 0) { vx_ = (v.x - lx_) / v.dt; vy_ = (v.y - ly_) / v.dt; vz_ = (v.z - lz_) / v.dt; }
        lx_ = v.x; ly_ = v.y; lz_ = v.z; have_last_ = true;
        track_people(v);
        const Leg* leg = nullptr;
        for (const Leg& l : legs_) if (v.t >= l.t0 && v.t < l.t1) { leg = &l; break; }
        // aim: the cast's middle, turned away and back on a panning leg
        double want_yaw = heading_of(v.fx - v.x, v.fy - v.y);
        const double flat = std::hypot(v.fx - v.x, v.fy - v.y);
        double want_pitch = std::atan2(v.fz - v.z, std::max(flat, 0.5)) * 180.0 / kPi;
        if (leg && leg->pan != 0) want_yaw += leg->pan * bump01((v.t - leg->t0) / (leg->t1 - leg->t0));
        mouse_.step(v.t, v.dt, v.yaw, v.pitch, want_yaw, want_pitch, 40.0, in.mouse_dx, in.mouse_dy);
        if (!mouse_.ready()) return in;   // no keys while calibrating
        Keys keys = leg ? leg->keys : Keys{};
        // the guard: where the camera will be in 0.8 s if these keys stay down
        if (v.t < guard_until_) keys = guard_keys_;
        else if (!safe_after(v, keys, 0.8)) {
            guard_keys_ = escape(v);
            guard_until_ = v.t + 0.7;
            ++guard_count_;
            keys = guard_keys_;
        }
        in.keys = keys;
        used_.w |= keys.w; used_.a |= keys.a; used_.s |= keys.s; used_.d |= keys.d; used_.q |= keys.q; used_.e |= keys.e;
        return in;
    }

    const Mouse& mouse() const { return mouse_; }
    Keys used() const { return used_; }
    int guard_count() const { return guard_count_; }

private:
    struct Leg { double t0, t1; Keys keys; double pan; };
    void step_of(const FlyView& v, const Keys& k, double& dx, double& dy, double& dz) const {
        const double h = v.yaw * kPi / 180.0;
        const double f = (k.w ? 1.0 : 0.0) - (k.s ? 1.0 : 0.0), r = (k.d ? 1.0 : 0.0) - (k.a ? 1.0 : 0.0);
        const double u = (k.e ? 1.0 : 0.0) - (k.q ? 1.0 : 0.0);
        double hx = -std::sin(h) * f + std::cos(h) * r, hy = std::cos(h) * f + std::sin(h) * r;
        const double n = std::hypot(hx, hy);
        if (n > 1e-9) { hx /= n; hy /= n; }
        dx = hx * room_.speed; dy = hy * room_.speed; dz = u * room_.climb;
    }
    // Everybody's pace, from where they were last frame: the guard looks ahead for
    // them too (the jogger closes 2 m in a second).
    void track_people(const FlyView& v) {
        std::vector<Seen> now;
        for (const Person& p : v.people) {
            Seen s{p.id, p.x, p.y, 0, 0};
            for (const Seen& o : seen_)
                if (o.id == p.id && v.dt > 0) {
                    s.vx = 0.8 * o.vx + 0.2 * (p.x - o.x) / v.dt;
                    s.vy = 0.8 * o.vy + 0.2 * (p.y - o.y) / v.dt;
                }
            now.push_back(s);
        }
        seen_ = std::move(now);
    }
    bool safe_at(double x, double y, double z, double ahead) const {
        if (y > room_.max_y || std::hypot(x - room_.cx, y - room_.cy) > room_.reach) return false;
        if (z < room_.min_z || z > room_.max_z) return false;
        for (const Seen& p : seen_)
            if (gap_to(x, y, z, p.x + p.vx * ahead, p.y + p.vy * ahead) < room_.clearance) return false;
        return true;
    }
    bool safe_after(const FlyView& v, const Keys& k, double) const {
        double dx, dy, dz;
        step_of(v, k, dx, dy, dz);
        // coasting on what it has -- for this frame at least: keys sent now act from
        // the next one, a whole 1/30 s at the recording's frame cap -- then easing
        // onto what the keys ask, looked at three times ahead: the camera needs
        // about a second to stop
        for (const double h : {0.4, 0.8, 1.2}) {
            const double c = h + v.dt;
            const double x = v.x + vx_ * c + (dx - vx_) * 0.5 * h, y = v.y + vy_ * c + (dy - vy_) * 0.5 * h;
            const double z = v.z + vz_ * c + (dz - vz_) * 0.5 * h;
            if (!safe_at(x, y, z, c)) return false;
        }
        return true;
    }
    // The keys that fly back toward the room: away from the nearest person (and up,
    // over their heads), back in front of the flags, in toward the stage, off the
    // ground or down from the top.
    Keys escape(const FlyView& v) const {
        double wx = 0, wy = 0, wz = 0;
        const Seen* closest = nullptr;   // not `near`: <windows.h> defines that away
        double best = 1e9;
        for (const Seen& p : seen_) {
            const double d = gap_to(v.x, v.y, v.z, p.x + p.vx * 0.6, p.y + p.vy * 0.6);
            if (d < best) { best = d; closest = &p; }
        }
        if (closest && best < room_.clearance + 1.5) {
            const double ax = v.x - (closest->x + closest->vx * 0.6), ay = v.y - (closest->y + closest->vy * 0.6);
            const double an = std::max(std::hypot(ax, ay), 0.1);
            wx += ax / an; wy += ay / an;
            if (v.z < 3.5) wz += 1.0;
        }
        if (v.y > room_.max_y - 1.0) wy -= 1.0;
        const double r = std::hypot(v.x - room_.cx, v.y - room_.cy);
        if (r > room_.reach - 1.5) { wx -= (v.x - room_.cx) / r; wy -= (v.y - room_.cy) / r; }
        if (v.z < room_.min_z + 0.5) wz += 1.0;
        if (v.z > room_.max_z - 0.5) wz -= 1.0;
        if (std::hypot(wx, wy) < 1e-6 && wz == 0) { wx = room_.cx - v.x; wy = (room_.cy - 7.0) - v.y; }   // back to the front
        const double h = v.yaw * kPi / 180.0;
        const double f = wx * -std::sin(h) + wy * std::cos(h), rt = wx * std::cos(h) + wy * std::sin(h);
        const double n = std::max(std::hypot(f, rt), 1e-9);
        Keys k;
        k.w = f / n > 0.35; k.s = f / n < -0.35; k.d = rt / n > 0.35; k.a = rt / n < -0.35;
        k.e = wz > 0; k.q = wz < 0;
        return k;
    }

    struct Seen { std::string id; double x, y, vx, vy; };
    FlyRoom room_;
    std::vector<Leg> legs_;
    std::vector<Seen> seen_;
    Mouse mouse_;
    Keys guard_keys_, used_;
    double guard_until_ = -1;
    int guard_count_ = 0;
    bool have_last_ = false;
    double lx_ = 0, ly_ = 0, lz_ = 0, vx_ = 0, vy_ = 0, vz_ = 0;
};

// ------------------------------------------------------------------ walk (C/D)

struct WalkView {
    double t = 0, dt = 1.0 / 60;        // s since control began; this frame's step
    double x = 0, y = 0, heading = 0;   // the played person
    double cam_yaw = 0, cam_pitch = 0;  // the game camera
    std::vector<Person> people;         // everybody else
};

// A member of the cast a rule watches, and how near sets it off.
struct Reach { std::string who; double metres = 0; };

// Standing a while, the camera comes round the person to its side or face (the customer, 2026-10-16:
// the third person as a player plays it, the camera following the mouse, not always behind), as a
// player looks the person over -- round to a side the follow camera's boom (stage_tape's chase: 4.35 m
// back from the person along the camera) keeps kBoomClearM off everybody, so nobody walks into it.
constexpr double kBoomM = 4.35, kBoomClearM = 1.5;
constexpr double kOrbitAfterS = 1.0;   // the body settled this long first
constexpr double kOrbitMinS = 6.0;     // a stand with this much left swings round; a shorter one stays behind
constexpr double kOrbitHoldS = 12.0;   // a long stand swings on to another side after this
constexpr double kStandCamRate = 45.0; // deg/s the camera turns at most, standing
// How near anybody comes to the boom of a camera looking `yaw` (stage-local) at the person.
inline double boom_clear(const WalkView& v, double yaw) {
    const double r = yaw * kPi / 180.0, bx = std::sin(r), by = -std::cos(r);   // back from the person, against the view
    double least = 1e9;
    for (double s = 1.0; s <= kBoomM + 1e-9; s += 0.25)
        for (const Person& p : v.people) least = std::min(least, std::hypot(v.x + bx * s - p.x, v.y + by * s - p.y));
    return least;
}
// The n-th stand's swing off the body: the first in turn whose boom is clear, else the clearest.
inline double pick_orbit(const WalkView& v, int n) {
    static const double kOrbits[8] = {80.0, -110.0, 0.0, 150.0, -70.0, 120.0, -150.0, 40.0};
    double best = 0.0, best_clear = -1.0;
    for (int i = 0; i < 8; ++i) {
        const double off = kOrbits[(n + i) % 8], c = boom_clear(v, v.heading + off);
        if (c >= kBoomClearM) return off;
        if (c > best_clear) { best_clear = c; best = off; }
    }
    return best;
}
// One stand's camera: behind the body till it has settled, then swung round (pick_orbit) if the stand
// has time for it, on to another side every kOrbitHoldS. `left_s`: how long the stand has to go.
class StandCam {
public:
    double want(const WalkView& v, bool settled, double left_s) {
        if (!settled) { settled_at_ = -1.0; return v.heading + (on_ ? orbit_ : 0.0); }
        if (settled_at_ < 0) settled_at_ = v.t;
        if (v.t - settled_at_ >= kOrbitAfterS && (!on_ || v.t - picked_at_ >= kOrbitHoldS)) {
            orbit_ = left_s > kOrbitMinS ? pick_orbit(v, n_++) : 0.0;
            on_ = true;
            picked_at_ = v.t;
        }
        return v.heading + (on_ ? orbit_ : 0.0);
    }
    void reset() { on_ = false; settled_at_ = -1.0; orbit_ = 0.0; }   // the stand is over
private:
    bool on_ = false;
    double orbit_ = 0.0, settled_at_ = -1.0, picked_at_ = 0.0;
    int n_ = 0;
};

struct WalkRoom {   // where a stroll goes: an arc in front of the stage, round the cast's middle
    double cx = 0.0, cy = 1.6;             // the stage centre: what a stroll watches
    double r_min = 7.0, r_max = 10.5;      // m from it
    // degrees round it from straight in front (-y), + toward +x; +40 keeps 2 m off the
    // jogger's lap. stage_tape's fixed camera (FIXED_AT) watches the arc from the left
    // side; its far left end, past 8 m out, is out of that view
    double phi_min = -28.0, phi_max = 40.0;
    double keep_off = 1.8;      // unchanged: this far from anybody, always (every rule's reach is under it)
    double pass_by = 1.2;       // changed: from anybody but the one walked up to
    // A stroll's walks (2026-10-09, the body turned first:
    // the walker sweep of stage_plan_selftest's model): steps in toward the cast, a little round the
    // arc each, and one walk back out; stops facing the cast between. The first person had people in
    // it 88% of the time (87% at worst), the view was 17 degrees off the body at most, the person
    // walked 13% of the time.
    // Stops of 9-12 s since 2026-10-13 (4-6 s before): a turn glided at 50 deg/s (TurnStyle) takes 3-4 s
    // where the game's took under one, and the longer stops make up the view it spends turning
    // (tools/route_sim on 003's cast, 200 runs: 87.9% on average, 85.3% at worst; 4-6 s stops, 79%).
    double step_min = 3.0, step_max = 7.0;      // degrees round the arc one walk goes
    int steps_in = 4;                           // walks in from r_max to r_min, toward the cast, before one back out
    double pause_min = 9.0, pause_max = 12.0;   // seconds a stop lasts
};

class Walker {
public:
    Walker(std::uint32_t seed, bool changed, double duration_s, std::vector<Reach> reaches, WalkRoom room = {}, TurnStyle turn = {})
        : changed_(changed), room_(room), turns_(turn) {
        std::mt19937 rng(seed);
        rng_ = rng;
        auto uni = [&](double a, double b) { return uniform(rng_, a, b); };
        if (changed_) {   // walk up to two or three of the watched, in a random order
            robot::shuffle(reaches.begin(), reaches.end(), rng_);
            const size_t n = std::min<size_t>(reaches.size(), 2 + rng_() % 2);
            for (size_t i = 0; i < n; ++i) goals_.push_back({true, 0, 0, reaches[i].who, 0.8 * reaches[i].metres, uni(4.0, 6.0)});
        }
        // Watching from in front of the stage: long stops facing the cast, and every so
        // often a short walk round an arc about the stage centre to the next spot on it,
        // now and then back the way it came. A walk looks the way it goes, across the line
        // to the cast (the customer, 2026-10-09: the first-person view goes with the face),
        // so the walks are short and the stops long: standing, the body turns to the cast
        // (stage_take hold_view), and the first person has people in it four fifths of the
        // time. The camera watches the stage centre as it walks, so it walks in on W, round
        // the arc on A or D and back out on S. It starts straight in front (phi 0).
        double phi = 0.0;
        int way = rng_() % 2 ? 1 : -1;
        const double step_in = (room_.r_max - room_.r_min) / room_.steps_in;
        for (int i = 0; i < 24; ++i) {   // out to r_max first (it starts 7.6 m out), then steps_in steps in, out, ...
            const int j = i % (room_.steps_in + 1);
            double next = phi + way * uni(room_.step_min, room_.step_max);
            if (next < room_.phi_min || next > room_.phi_max) { way = -way; next = phi + way * uni(room_.step_min, room_.step_max); }
            phi = std::clamp(next, room_.phi_min, room_.phi_max);
            const double r = room_.r_max - j * step_in + uni(-0.1, 0.1);
            goals_.push_back({false, 0, 0, "", 0.5, uni(room_.pause_min, room_.pause_max), true, phi, r});
            if (rng_() % 3 == 0) way = -way;
        }
        (void)duration_s;
    }

    Input step(const WalkView& v) {
        Input in;
        const double look_pitch = kLookPitchDeg + 3.0 * std::sin(v.t * 0.37);
        const double body_rate = seen_ && v.dt > 0 ? wrap(v.heading - last_heading_) / v.dt : 0.0;
        last_heading_ = v.heading;
        seen_ = true;
        if (!mouse_.ready()) {
            mouse_.step(v.t, v.dt, v.cam_yaw, v.cam_pitch, v.cam_yaw, look_pitch, 60.0, in.mouse_dx, in.mouse_dy);
            return in;   // still: no keys while calibrating
        }
        // the goal and where it is
        while (goal_ < goals_.size() && goals_[goal_].meet && !find(v, goals_[goal_].who)) { ++goal_; goal_t0_ = v.t; }
        if (goal_ >= goals_.size()) { mouse_.step(v.t, v.dt, v.cam_yaw, v.cam_pitch, v.heading, look_pitch, 60.0, in.mouse_dx, in.mouse_dy); return in; }
        Goal& g = goals_[goal_];
        // a stroll watches the stage centre -- the cast is round it -- so the first
        // person keeps people in view
        const double watch = heading_of(room_.cx - v.x, room_.cy - v.y);
        double gx = g.x, gy = g.y, dist = 0.0;
        if (g.meet) { const Person* p = find(v, g.who); gx = p->x; gy = p->y; }
        if (g.arc) {   // a spot round the arc, walked to straight: a walk is short, under 0.2 m inside the arc
            gx = room_.cx + g.r * std::sin(g.phi * kPi / 180.0);
            gy = room_.cy - g.r * std::cos(g.phi * kPi / 180.0);
        }
        dist = std::hypot(gx - v.x, gy - v.y);
        double want = heading_of(gx - v.x, gy - v.y);
        bool walk = true;
        double look = want;   // where the person would look, standing
        // A stopped stroll skips a goal inside its start-up travel margin.
        // Starting a few millimetres outside the arrival radius otherwise creates
        // one-frame W taps. Meetings and all immediate safety stops stay unchanged.
        const double arrival_radius = g.radius + ((!changed_ && g.arc && !moving_) ? 0.4 : 0.0);
        if (pause_until_ > v.t) {   // arrived: stand and look at whom it met, or at the cast
            walk = false;
            const Person* met = met_goal_ ? find(v, met_who_) : nullptr;
            look = met ? heading_of(met->x - v.x, met->y - v.y) : watch;
        } else if (dist < arrival_radius || (g.meet && v.t - goal_t0_ > 14.0) ||
                   (moving_ && dist < kPassM && std::fabs(wrap(want - v.heading)) > kOffWayDeg)) {
            // there, or just past it (or the one walked up to kept away)
            if (g.meet && dist < g.radius) ++met_;
            met_goal_ = g.meet;
            met_who_ = g.who;
            pause_until_ = v.t + g.pause;
            ++goal_;
            goal_t0_ = v.t;
            walk = false;
            look = g.meet ? want : watch;
        }
        // somebody in the way: stop and wait; stuck a while: step round them
        const double keep = changed_ ? room_.pass_by : room_.keep_off;
        bool blocked = false;
        for (const Person& p : v.people) {
            if (g.meet && p.id == g.who) continue;
            const double d = std::hypot(p.x - v.x, p.y - v.y);
            // once waiting, on only when they are well clear: somebody at the edge of the
            // reach stopped and started the person every few steps (2026-09-28)
            const double reach = keep + (waiting_ ? 0.8 : 0.4);
            if (d < reach && std::fabs(wrap(heading_of(p.x - v.x, p.y - v.y) - want)) < (waiting_ ? 75.0 : 60.0)) blocked = true;
        }
        waiting_ = walk && blocked && v.t >= detour_until_;
        if (walk && v.t < detour_until_) want = detour_;
        else if (walk && blocked) {
            if (blocked_since_ < 0) blocked_since_ = v.t;
            if (v.t - blocked_since_ > 2.5) { detour_ = want + ((rng_() % 2) ? 70.0 : -70.0); detour_until_ = v.t + 1.5; blocked_since_ = -1; }
            walk = false;
            look = g.meet ? want : watch;   // waiting on a stroll: watch the cast meanwhile
        } else if (!blocked) {
            blocked_since_ = -1;
        }
        // The body first: standing, it turns to what it watches, or to the way it is about to walk.
        // The camera comes round after the body while it stands; before a walk it turns to the way less
        // the keys' way -- the keys nearest the way off the stage centre, so the camera keeps watching
        // the stage -- and walking, it steers the way that much off.
        if (!moving_) {   // a way between two keys' ways keeps the keys it had (or the camera swung 45 degrees to and fro)
            const Keys k = keys_for(wrap(want - watch));
            if (std::fabs(wrap(want - watch - key_way(k))) + 10.0 < std::fabs(wrap(want - watch - leg_off_))) { leg_keys_ = k; leg_off_ = key_way(k); }
        }
        if (moving_) {   // along the way, a hand's small sway
            turns_.reset();
            mouse_.step(v.t, v.dt, v.cam_yaw, v.cam_pitch, want - leg_off_ + 2.0 * std::sin(v.t * 0.9), look_pitch, 60.0, in.mouse_dx, in.mouse_dy);
        } else if (walk) {
            stand_cam_.reset();
            in.body_to = turns_.to(v.t, v.dt, v.heading, body_rate, want, v.people, v.x, v.y);
            mouse_.step(v.t, v.dt, v.cam_yaw, v.cam_pitch, want - leg_off_, look_pitch, 60.0, in.mouse_dx, in.mouse_dy);
        } else {   // standing: the camera after the body and then round it (StandCam)
            in.body_to = turns_.to(v.t, v.dt, v.heading, body_rate, look, v.people, v.x, v.y);
            const bool settled = std::fabs(wrap(look - v.heading)) < kAlignDeg && std::fabs(body_rate) < kSettledDegS;
            mouse_.step(v.t, v.dt, v.cam_yaw, v.cam_pitch, stand_cam_.want(v, settled, pause_until_ - v.t), look_pitch, kStandCamRate,
                        in.mouse_dx, in.mouse_dy);
        }
        // Keys that came and went within a second shuffled the body on the spot (2026-09-28):
        // once stopped, the person stands a moment before it sets off again.
        const bool may_go = moving_ || v.t - stopped_at_ >= kMinStandS;
        // Walking, the way turns no tighter than the camera turns, 1.6 m round at walking speed: a goal
        // that comes round off the way is inside that and would be circled for ever (beach-demo-003 D,
        // 2026-10-09: 81 s round one) -- the person stops, turns to it standing, and goes on.
        const bool facing = std::fabs(wrap(want - v.heading)) < kAlignDeg && std::fabs(wrap(want - leg_off_ - v.cam_yaw)) < kAlignDeg &&
                            std::fabs(body_rate) < kSettledDegS;
        const bool off_way = std::fabs(wrap(want - v.heading)) > kOffWayDeg;
        if (walk && may_go && (facing || (moving_ && !off_way))) in.keys = leg_keys_;
        // once set off, half a second of it at least: the camera lagging a turning body
        // let go of the keys a few frames in (arriving or somebody in the way still stop it)
        if (moving_ && walk && !blocked && !in.keys.any() && v.t - moving_since_ < kMinWalkS) in.keys = last_keys_;
        // a stroll never walks into anybody's rule; stop dead instead
        if (!changed_ && in.keys.any()) {   // the step goes where the keys send it off the camera
            const double step = (v.cam_yaw + key_way(in.keys)) * kPi / 180.0;
            double hx = -std::sin(step), hy = std::cos(step);
            for (const Person& p : v.people)
                if (std::hypot(v.x + 0.8 * hx - p.x, v.y + 0.8 * hy - p.y) < room_.keep_off) in.keys = Keys{};
        }
        if (moving_ && !in.keys.any()) stopped_at_ = v.t;
        if (!moving_ && in.keys.any()) moving_since_ = v.t;
        moving_ = in.keys.any();
        last_keys_ = in.keys;
        return in;
    }

    const Mouse& mouse() const { return mouse_; }
    int met() const { return met_; }
    size_t goals_done() const { return goal_; }
    int turns() const { return turns_.turns(); }

private:
    struct Goal {
        bool meet; double x, y; std::string who; double radius, pause;
        bool arc = false; double phi = 0, r = 0;   // a spot on the stroll's arc (WalkRoom): angle and distance
    };
    static const Person* find(const WalkView& v, const std::string& who) {
        for (const Person& p : v.people) if (p.id == who) return &p;
        return nullptr;
    }
    bool changed_;
    WalkRoom room_;
    std::mt19937 rng_;
    std::vector<Goal> goals_;
    size_t goal_ = 0;
    Mouse mouse_;
    double pause_until_ = -1, goal_t0_ = 0, blocked_since_ = -1, detour_until_ = -1, detour_ = 0;
    static constexpr double kMinStandS = 0.6;   // stopped, this long before setting off again
    static constexpr double kMinWalkS = 0.5;    // set off, this long before the keys may go
    static constexpr double kOffWayDeg = 50.0, kPassM = 1.0;   // the goal this far off the way, this near: walked past it
    double moving_since_ = -10.0, stopped_at_ = -10.0, last_heading_ = 0.0;
    bool seen_ = false;   // last_heading_ is the body's
    bool waiting_ = false, moving_ = false;   // stopped for somebody in the way; keys down
    Keys last_keys_;
    Keys leg_keys_ = keys_for(0.0);   // the keys a walk goes on, and their way off the camera
    double leg_off_ = 0.0;
    StandCam stand_cam_;
    Turner turns_;
    bool met_goal_ = false;   // the goal last reached was a meeting: look at that person while standing
    std::string met_who_;
    int met_ = 0;
};


// ------------------------------------------------------------------ route (C)

// One spot of a route: walk there no earlier than stage second `from_s`, then stand
// `stand_s` looking at `look` (an actor, or "-" for the stage's middle). A spot with no
// stand is a way point: walked round, not stopped at, so a route bends instead of
// turning the person on the spot -- gently: a bend over ~45 degrees swings out a metre
// (the walk turns no tighter than 1.6 m round). Turning further is a stop and a start.
// Then, maybe, the keys the leg to the spot goes on: w, a, s, d, wa, wd, sa or sd (the customer,
// 2026-10-16: not W alone -- in the third person A, S and D turn the person to that way off the
// camera and walk it); "-" or left out, the ones nearest the way off where the camera looks as the
// leg begins. The camera turns to the way less the keys' way before they go down. And maybe how:
//   tap   the keys tapped (RouteWalker::kTapS), the person standing where it is: the game turns the
//         body toward (x, y) -- kTapMaxDeg at most -- with a short step (0.23-0.33 m), then it stands
//         stand_s. With the walk turn style it is how a standing person turns to look at somebody.
//   step  the keys held RouteWalker::kStepS toward (x, y), however near or far: one step, 1.2-1.9 m.
// Or, instead of the keys, a key timetable (parse_timetable, "w:2,wd:1,sd:1,s:1"): the camera set behind the body,
// then each combination held its seconds in turn with the mouse untouched, then a slide and a stand like any leg's
// end -- (x, y) is only where the walk is meant to end (the customer, 2026-10-16: a big turn walked round as W, W+D,
// S+D, S; win-pc measures how the game walks it before routes are made of it).
struct RouteStep {
    enum class How { Walk, Tap, Step, Timetable };
    double from_s = 0, x = 0, y = 0, stand_s = 0;
    std::string look, keys;   // keys "" -- the nearest
    How how = How::Walk;
    std::vector<std::pair<Keys, double>> timetable;   // How::Timetable
};

// "from_s x y stand_s look; ..." into steps; "" or what is wrong with it.
inline std::string parse_route(const std::string& text, std::vector<RouteStep>& out) {
    out.clear();
    std::stringstream all(text);
    std::string item;
    double last = -1e9;
    while (std::getline(all, item, ';')) {
        if (item.find_first_not_of(" \t\r\n") == std::string::npos) continue;
        std::istringstream in(item);
        RouteStep r;
        if (!(in >> r.from_s >> r.x >> r.y >> r.stand_s >> r.look))
            return "route step \"" + item + "\": five fields, from_s x y stand_s look, then maybe the keys and tap or step";
        bool keyed = false, how = false;
        for (std::string word; in >> word;) {
            Keys k;
            if (!keyed && !how && parse_timetable(word, r.timetable)) {   // a timetable is the keys and the how at once
                keyed = how = true;
                r.keys = word;
                r.how = RouteStep::How::Timetable;
            } else if ((word == "tap" || word == "step") && !how) {
                how = true;
                r.how = word == "tap" ? RouteStep::How::Tap : RouteStep::How::Step;
            } else if (!keyed && !how && (word == "-" || parse_keys(word, k))) {
                keyed = true;
                r.keys = word == "-" ? "" : word;
            } else {
                return "route step \"" + item + "\": after the five fields the keys (w, a, s, d, wa, wd, sa, sd or -), then tap or step; "
                       "or a key timetable (w:2,wd:1,sd:1,s:1)";
            }
        }
        if (!std::isfinite(r.from_s) || !std::isfinite(r.x) || !std::isfinite(r.y) || !std::isfinite(r.stand_s) ||
            r.stand_s < 0 || std::fabs(r.x) > 30.0 || std::fabs(r.y) > 30.0)
            return "route step \"" + item + "\": out of range";
        if (r.from_s < last) return "route steps must come in time order";
        last = r.from_s;
        out.push_back(r);
    }
    return out.empty() ? "the route has no steps" : "";
}

// Walks a route the way the game walks a person. Measured on the take that went wrong
// (beach-demo-003 C, 2026-09-28: it slid into the chatting pair and then circled two
// spots for half a minute each): W walks where the camera points -- for the first
// ~1.1 s of a start straight where the camera pointed as W went down (the game's own
// start, whatever the camera does meanwhile; turned round first if the body faced away,
// on the spot -- twelve starts of 105-180 degrees in the earlier takes, all within 7
// degrees of the camera), then ~0.3 s behind the camera -- so a bend is only as tight as
// the camera turns (60 deg/s at 1.7 m/s: a 1.6 m radius; a spot inside it is circled for
// ever); and let go, the person slides on 0.5-1.5 m (14 stops in four takes, 0.93 on
// average, sideways under 0.1 m unless it was turning). So a route is a path and is walked like one: the camera
// looks kAheadM along it, bends come out round, W goes down only once the camera
// points along the path's first stretch, and lets go one slide before a spot. A spot
// missed stays missed -- stepping back and forth is the shuffle -- so a route keeps its
// distances with the slide's spread in them; the stage sim checks that before anybody
// records. Standing, the take turns the body to what it looks at (Input::body_to) and the
// camera comes round after it; before a leg the body turns to its way and the camera to the way
// less the keys' way (RouteStep), and the keys go down. Somebody within kGuardM of the path's next kGuardAheadM stops the walk; it goes on
// once they are clear. After the last spot it stands, looking where that spot looks.
// With the walk turn style the program never turns the body: a leg's keys go down with the camera
// aimed (the game turns the body round to the way as it sets off, on the spot), and a stand keeps the
// body where the walk or a tap left it. A key timetable is held as written, the mouse untouched.
class RouteWalker {
public:
    RouteWalker(std::vector<RouteStep> steps, double start_s, WalkRoom room = {}, TurnStyle turn = {})
        : steps_(std::move(steps)), start_s_(start_s), room_(room), turns_(turn), walk_turn_(turn.mode == TurnMode::Walk),
          straight_(turn.mode == TurnMode::Walk && turn.straight),
          cam_rate_(turn.mode == TurnMode::Walk ? std::min(kTurnRate, turn.rate) : kTurnRate) {}

    Input step(const WalkView& v) {
        Input in;
        const double stage_s = start_s_ + v.t;
        if (have_last_ && v.dt > 0)   // how fast it goes: a slide is over once it stands still
            speed_ += (std::hypot(v.x - last_x_, v.y - last_y_) / v.dt - speed_) * std::min(1.0, v.dt / 0.15);
        last_x_ = v.x; last_y_ = v.y;
        const double body_rate = have_last_ && v.dt > 0 ? wrap(v.heading - last_heading_) / v.dt : 0.0;
        last_heading_ = v.heading; have_last_ = true;
        const double look_pitch = kLookPitchDeg + 3.0 * std::sin(v.t * 0.37);
        if (!mouse_.ready()) {
            mouse_.step(v.t, v.dt, v.cam_yaw, v.cam_pitch, v.cam_yaw, look_pitch, kTurnRate, in.mouse_dx, in.mouse_dy);
            return in;   // still: no keys while calibrating
        }
        double want_cam = v.cam_yaw, cam_rate = cam_rate_;
        switch (state_) {
        case State::Stand:
            if (next_ < steps_.size() && v.t >= stand_until_ && stage_s >= steps_[next_].from_s &&
                steps_[next_].how == RouteStep::How::Timetable) {   // the camera behind the body, then the timetable
                state_ = State::TimedAim;
                aim_since_ = v.t;
                stand_cam_.reset();
                want_cam = v.heading;
                break;
            }
            if (next_ < steps_.size() && v.t >= stand_until_ && stage_s >= steps_[next_].from_s && steps_[next_].how == RouteStep::How::Tap) {
                begin_tap(v);
                if (state_ == State::Tap) { want_cam = tap_cam_; break; }
                break;   // no tap needed: it stands on
            }
            if (next_ < steps_.size() && v.t >= stand_until_ && stage_s >= steps_[next_].from_s) {
                plan_leg(v);
                if (state_ == State::Stand) break;   // the next spot is where it stands: only the look changes
            } else {   // standing: the body turns to what it looks at, the camera after it and then round it (StandCam)
                const double look = look_way(v);
                in.body_to = turns_.to(v.t, v.dt, v.heading, body_rate, look, v.people, v.x, v.y);
                const bool settled = (walk_turn_ || std::fabs(wrap(look - v.heading)) < kAlignDeg) && std::fabs(body_rate) < kSettledDegS;
                const double left_s = next_ < steps_.size() ? std::max(stand_until_, steps_[next_].from_s - start_s_) - v.t : 1e9;
                want_cam = stand_cam_.want(v, settled, left_s);
                cam_rate = kStandCamRate;
                break;
            }
            [[fallthrough]];
        case State::Aim: {   // the body turns to the leg's first stretch, the camera to it less the keys' way, then the keys
            const double way = start_way(v);
            if (walk_turn_ && std::fabs(wrap(way - v.heading)) > kStartTurnMaxDeg && std::fabs(body_rate) < kSettledDegS &&
                v.t - stopped_at_ >= kMinStandS) {   // too far round for a start: a tap toward it first, then aim again
                const double to = wrap(v.heading + std::clamp(wrap(way - v.heading), -kTapMaxDeg, kTapMaxDeg));
                tap_keys_ = key_for(wrap(to - v.cam_yaw));
                tap_cam_ = wrap(to - key_way(tap_keys_));
                aim_since_ = v.t;
                tap_then_aim_ = true;
                state_ = State::Tap;
                want_cam = tap_cam_;
                break;
            }
            in.body_to = turns_.to(v.t, v.dt, v.heading, body_rate, way, v.people, v.x, v.y);
            want_cam = way - leg_off_;
            const double aim = std::fabs(wrap(way - leg_off_ - v.cam_yaw));
            bool ready;
            if (stepping_) {   // a step goes where the keys point as they go down, a metre and more: aimed finely, held
                if (aim >= kFineAimDeg) aim_since_ = v.t;
                ready = aim < kFineAimDeg && v.t - aim_since_ >= kFineAimS;
            } else {
                ready = aim < kAimDeg;
            }
            // the body to the way first and settled -- or, the walk style, left for the game to turn as the keys go down
            // (kStartTurnMaxDeg at most, taps before), once a tap's turn has settled
            ready = ready && (walk_turn_ ? std::fabs(body_rate) < kSettledDegS
                                        : std::fabs(wrap(way - v.heading)) < kAlignDeg && std::fabs(body_rate) < kSettledDegS);
            if (ready && v.t - stopped_at_ >= kMinStandS && clear(v, 0.0, kGuardM)) {
                in.body_to = std::nan("");
                turns_.reset();
                state_ = State::Walk;
                pressed_at_ = v.t;
                start_turn_ = std::max(start_turn_, std::fabs(wrap(v.cam_yaw + leg_off_ - v.heading)));
                in.keys = leg_keys_;
            }
            break;
        }
        case State::Walk: {
            progress(v);
            const auto ahead = at(std::min(done_ + kAheadM, length()));
            want_cam = heading_of(ahead.first - v.x, ahead.second - v.y) - leg_off_ + 2.0 * std::sin(v.t * 0.9);
            const double left = length() - done_;
            const double slide = kSlideM * std::clamp(speed_ / kWalkMps, 0.3, 1.0);
            in.keys = leg_keys_;
            if (stepping_ ? v.t - pressed_at_ >= kStepS : v.t - pressed_at_ >= kMinWalkS && left <= slide) {
                in.keys = Keys{};
                state_ = State::Slide;
                held_ = false;
            } else if (!clear(v, done_, kGuardM)) {
                in.keys = Keys{};
                state_ = State::Slide;
                held_ = true;
                ++held_back_;
            }
            if (state_ == State::Slide) released_at_ = v.t;
            break;
        }
        case State::Slide:   // keys up; the camera stays where it looked until the person stands
            if (speed_ < kStillMps || v.t - released_at_ > kSlideMaxS) {
                stopped_at_ = v.t;
                if (held_) {
                    state_ = State::Held;
                    clear_since_ = -1.0;
                } else {
                    const RouteStep& s = steps_[spot_];
                    if (std::hypot(s.x - v.x, s.y - v.y) > kMissM) ++missed_;
                    ++arrived_;
                    look_ = s.look;
                    stand_until_ = v.t + s.stand_s;
                    next_ = spot_ + 1;
                    state_ = State::Stand;
                }
            }
            break;
        case State::Tap:   // the camera set off the way by the keys' way, held there, then the keys go down
            want_cam = tap_cam_;
            if (std::fabs(wrap(tap_cam_ - v.cam_yaw)) >= kFineAimDeg) aim_since_ = v.t;
            if (std::fabs(wrap(tap_cam_ - v.cam_yaw)) < kFineAimDeg && v.t - aim_since_ >= kFineAimS && v.t - stopped_at_ >= kMinStandS) {
                state_ = State::TapPress;
                pressed_at_ = v.t;
                ++taps_;
                in.keys = tap_keys_;
                taps_keys_.push_back(tap_keys_);
            }
            break;
        case State::TapPress:   // held kTapS and let go: the game turns the body round with a short step
            want_cam = tap_cam_;
            if (v.t - pressed_at_ < kTapS) {
                in.keys = tap_keys_;
            } else if (tap_then_aim_) {   // a tap before a leg: aim again once the body has come round
                stopped_at_ = v.t;
                tap_then_aim_ = false;
                aim_since_ = v.t;
                state_ = State::Aim;
            } else if (tap_more_) {   // still too far round: stand till the body has come round, then tap again
                stopped_at_ = v.t;
                tap_more_ = false;
                stand_until_ = v.t + kTapSettleS;
                state_ = State::Stand;
            } else {
                const RouteStep& s = steps_[next_];
                step_taps_ = 0;
                stopped_at_ = v.t;
                ++arrived_;
                look_ = s.look;
                stand_until_ = v.t + s.stand_s;
                ++next_;
                state_ = State::Stand;
            }
            break;
        case State::TimedAim:   // the camera straight behind the settled body, held a moment
            want_cam = v.heading;
            if (std::fabs(wrap(v.heading - v.cam_yaw)) >= kFineAimDeg || std::fabs(body_rate) >= kSettledDegS) aim_since_ = v.t;
            if (v.t - aim_since_ >= kFineAimS && v.t - stopped_at_ >= kMinStandS) {
                state_ = State::Timed;
                pressed_at_ = v.t;
                spot_ = next_;
                for (const auto& entry : steps_[spot_].timetable) legs_keys_.push_back(entry.first);
                mouse_.still();
                in.keys = steps_[spot_].timetable.front().first;
                return in;   // the mouse untouched from here: whatever the camera does, the game does
            }
            break;
        case State::Timed: {   // each combination held its seconds in turn, then keys up: a slide and a stand
            double into = v.t - pressed_at_;
            for (const auto& entry : steps_[spot_].timetable) {
                if (into < entry.second) { in.keys = entry.first; return in; }
                into -= entry.second;
            }
            state_ = State::Slide;
            held_ = false;
            released_at_ = v.t;
            mouse_.still();
            return in;
        }
        case State::Held:   // somebody was in the way: on along the rest of the leg once they are clear
            progress(v);
            want_cam = v.cam_yaw;
            // clear by a margin, so the walk does not stop again at once -- or, after a while,
            // clear at all: somebody standing just off the path must not hold it for ever
            if (!clear(v, done_, v.t - stopped_at_ < kHeldS ? kClearM : kGuardM)) clear_since_ = -1.0;
            else if (clear_since_ < 0) clear_since_ = v.t;
            if (clear_since_ >= 0 && v.t - clear_since_ >= 0.5) {
                std::vector<std::pair<double, double>> rest{{v.x, v.y}};
                for (size_t i = 1; i < leg_.size(); ++i) if (cum_[i] > done_ + 0.3) rest.push_back(leg_[i]);
                if (rest.size() < 2) rest.push_back(leg_.back());
                set_leg(rest);
                state_ = State::Aim;
            }
            break;
        }
        mouse_.step(v.t, v.dt, v.cam_yaw, v.cam_pitch, want_cam, look_pitch, cam_rate, in.mouse_dx, in.mouse_dy);   // as a hand turns it
        return in;
    }

    const Mouse& mouse() const { return mouse_; }
    size_t arrived() const { return arrived_; }     // spots reached (or stopped short of)
    int missed() const { return missed_; }          // of them, stopped more than kMissM off
    int held_back() const { return held_back_; }    // walks stopped for somebody in the way
    double start_turn() const { return start_turn_; }   // the most the body was off its way as the keys went down, degrees
    const std::vector<Keys>& legs_keys() const { return legs_keys_; }   // the keys each leg went on
    const std::vector<Keys>& taps_keys() const { return taps_keys_; }   // ... and each tap
    int taps() const { return taps_; }
    int turns() const { return turns_.turns(); }   // standing turns of more than a step (steps, glide)

    static constexpr double kWalkMps = 1.72;   // walking speed (measured)
    static constexpr double kSlideM = 0.93;    // the slide after letting go at walking speed: 0.52-1.51 m, 0.93 on average (14 stops, 4 takes)
    static constexpr double kStartM = 1.45;    // how far a start goes straight where the camera pointed (1.1 s)
    static constexpr double kTurnRate = 60.0;  // deg/s the camera turns at most
    // A tap (RouteStep::How::Tap): the keys held this long -- held 0.5 s, the game walked a metre as often as it
    // turned on the spot -- turning the body this far at most (win-pc, 2026-10-15: 0.43 s taps 30-90 degrees off the
    // body, the body round within a degree in ~1.4 s at 44/70/100/165 deg/s at the fastest, 0.24-0.33 m moved).
    // No tap for less than kTapSkipDeg: at 35 mm the first person is 54 degrees wide, so somebody 25 off is in it (the
    // taps that walked a metre, 4 of 6 on win-pc 2026-10-17, were all on two keys: taps go on one now, key_for).
    static constexpr double kTapS = 0.43, kTapMaxDeg = 45.0, kTapSkipDeg = 25.0;
    // a look further round than kTapMaxDeg + kTapSkipDeg takes another tap once the body has come round (~1.4 s from the
    // press), kTapsMax taps at most
    static constexpr double kTapSettleS = 1.5;
    static constexpr int kTapsMax = 3;
    // The walk style turns the body no further at a key than these (the user, 2026-10-16: the first person goes with the
    // head with no lag, so the body must not swing fast): a tap 45 degrees (70 deg/s at its fastest), a start 35 (about
    // 60) -- a leg further off the body than kStartTurnMaxDeg is turned to with taps first. Recorded so on win-pc
    // (2026-10-16, C4 and D6): the first person never over 90 deg/s, 73 at the fastest, its turn eased within 150 deg/s^2.
    static constexpr double kStartTurnMaxDeg = 35.0;
    // A step (RouteStep::How::Step): the keys held this long from standing (18 frames at 30), a walk for sure. A short
    // walk stops one of two ways, by where the feet are as the key comes up: at once (0.53 s: 1.16 m in all, 0.57 s:
    // 1.25 m) or a stride later (0.60 s: 1.61 and 1.87 m; win-pc, 2026-10-15), so a step goes 1.2-1.9 m.
    static constexpr double kStepS = 0.58;

private:
    enum class State { Stand, Aim, Walk, Slide, Held, Tap, TapPress, TimedAim, Timed };
    static constexpr double kAheadM = 1.3;     // the camera looks this far along the path
    static constexpr double kAimDeg = 4.0;     // W goes down with the camera this near the path's first stretch
    // a tap or a step goes where the camera and keys point as they go down: aimed this finely and held so long (4 degrees
    // let steps go 9 degrees off, win-pc 2026-10-15)
    static constexpr double kFineAimDeg = 1.5, kFineAimS = 0.3;
    // a walk holds its keys this long at least: let go at 0.5 s the game walked a metre as often as it tapped
    // (win-pc, 2026-10-15) -- the same as a step
    static constexpr double kMinStandS = 0.6, kMinWalkS = kStepS;
    static constexpr double kStillMps = 0.12, kSlideMaxS = 1.8;
    static constexpr double kMissM = 0.5;
    static constexpr double kGuardM = 0.7, kGuardAheadM = 2.0, kClearM = 0.9, kHeldS = 3.0;
    static constexpr double kMinLegM = 0.6;    // a spot this near is stood on where the person is

    // A tap toward the next step's spot: the body turned that way, kTapMaxDeg at most, on the keys the route
    // names or the single key nearest it off the camera (key_for: two keys walk); the camera set off it by the keys' way.
    void begin_tap(const WalkView& v) {
        const RouteStep& s = steps_[next_];
        const double off = wrap(heading_of(s.x - v.x, s.y - v.y) - v.heading);
        if (std::fabs(off) < kTapSkipDeg) {   // facing it already: a tap would only shuffle it 0.3 m
            step_taps_ = 0;
            stand_cam_.reset();
            ++arrived_;
            look_ = s.look;
            stand_until_ = v.t + s.stand_s;
            ++next_;
            return;
        }
        const double to = wrap(v.heading + std::clamp(off, -kTapMaxDeg, kTapMaxDeg));
        // further round than one tap goes: another once the body has come round, kTapsMax in all (beach-demo-102's D,
        // win-pc 2026-10-17: a walk ended 93 degrees off the people it was to look at, one tap left it 49 off, 14 s on
        // empty sand)
        tap_more_ = std::fabs(off) > kTapMaxDeg + kTapSkipDeg && ++step_taps_ < kTapsMax;
        if (!parse_keys(s.keys, tap_keys_)) tap_keys_ = key_for(wrap(to - v.cam_yaw));
        tap_cam_ = wrap(to - key_way(tap_keys_));
        aim_since_ = v.t;
        stand_cam_.reset();
        state_ = State::Tap;
    }
    // The next leg: from where the person stands through the way points to the next spot -- a step, straight at it.
    void plan_leg(const WalkView& v) {
        std::vector<std::pair<double, double>> pts{{v.x, v.y}};
        size_t i = next_;
        stepping_ = steps_[i].how == RouteStep::How::Step;
        while (!stepping_ && i + 1 < steps_.size() && steps_[i].stand_s <= 0.0) pts.push_back({steps_[i].x, steps_[i].y}), ++i;
        pts.push_back({steps_[i].x, steps_[i].y});
        spot_ = i;
        set_leg(pts);
        // the leg's keys: the route's, or the ones nearest its way off where the camera now looks
        if (!parse_keys(steps_[i].keys, leg_keys_)) leg_keys_ = keys_for(wrap(start_way(v) - v.cam_yaw));
        leg_off_ = key_way(leg_keys_);
        stand_cam_.reset();
        aim_since_ = v.t;
        if (length() < kMinLegM && !stepping_) {
            ++arrived_;
            look_ = steps_[i].look;
            stand_until_ = v.t + steps_[i].stand_s;
            next_ = i + 1;
            state_ = State::Stand;
            return;
        }
        legs_keys_.push_back(leg_keys_);
        state_ = State::Aim;
    }
    double first_way(const WalkView& v) const {   // where the start will walk the person, straight
        const auto first = at(std::min(kStartM, length()));
        return heading_of(first.first - v.x, first.second - v.y);
    }
    // where a leg sets off: its first stretch -- or, walk ... straight, the way the body faces when that is near enough:
    // the walk comes round onto the leg after (the camera looks kAheadM along it)
    double start_way(const WalkView& v) const {
        const double way = first_way(v);
        return straight_ && std::fabs(wrap(way - v.heading)) <= kStraightStartDeg ? v.heading : way;
    }
    void set_leg(const std::vector<std::pair<double, double>>& pts) {
        leg_ = pts;
        cum_.assign(1, 0.0);
        for (size_t i = 1; i < leg_.size(); ++i)
            cum_.push_back(cum_.back() + std::hypot(leg_[i].first - leg_[i - 1].first, leg_[i].second - leg_[i - 1].second));
        done_ = 0.0;
        seg_ = 0;
    }
    double length() const { return cum_.empty() ? 0.0 : cum_.back(); }
    std::pair<double, double> at(double s) const {   // the point s metres along the leg
        for (size_t i = 1; i < leg_.size(); ++i) {
            if (s <= cum_[i] || i + 1 == leg_.size()) {
                const double l = cum_[i] - cum_[i - 1], f = l > 1e-9 ? std::clamp((s - cum_[i - 1]) / l, 0.0, 1.0) : 1.0;
                return {leg_[i - 1].first + f * (leg_[i].first - leg_[i - 1].first), leg_[i - 1].second + f * (leg_[i].second - leg_[i - 1].second)};
            }
        }
        return leg_.empty() ? std::pair<double, double>{0.0, 0.0} : leg_.back();
    }
    // How far along the leg the person is: its nearest point on this stretch or the next
    // two, never back.
    void progress(const WalkView& v) {
        double best = 1e18, best_s = done_;
        size_t best_seg = seg_;
        for (size_t i = seg_; i + 1 < leg_.size() && i < seg_ + 3; ++i) {
            const double ax = leg_[i].first, ay = leg_[i].second, bx = leg_[i + 1].first, by = leg_[i + 1].second;
            const double l2 = (bx - ax) * (bx - ax) + (by - ay) * (by - ay);
            const double f = l2 > 1e-12 ? std::clamp(((v.x - ax) * (bx - ax) + (v.y - ay) * (by - ay)) / l2, 0.0, 1.0) : 0.0;
            const double d = std::hypot(ax + f * (bx - ax) - v.x, ay + f * (by - ay) - v.y);
            if (d < best - 1e-9) { best = d; best_s = cum_[i] + f * std::sqrt(l2); best_seg = i; }
        }
        if (best_s >= done_) { done_ = best_s; seg_ = best_seg; }
    }
    // Nobody the path from `from` to kGuardAheadM beyond it takes the person within `reach`
    // of -- and nearer than it is now: walking away from somebody close is no harm.
    bool clear(const WalkView& v, double from, double reach) const {
        for (double s = from; s <= std::min(from + kGuardAheadM, length()) + 1e-9; s += 0.1) {
            const auto p = at(s);
            for (const Person& q : v.people) {
                const double then = std::hypot(q.x - p.first, q.y - p.second);
                if (then < reach && then < std::hypot(q.x - v.x, q.y - v.y) - 0.05) return false;
            }
        }
        return true;
    }
    double look_way(const WalkView& v) const {   // where the standing person looks: its spot's look, else the stage
        if (look_ == "-" && walk_turn_) return v.heading;   // the walk style: where the walk or the tap left it
        double look = heading_of(room_.cx - v.x, room_.cy - v.y);
        for (const Person& p : v.people) if (p.id == look_) look = heading_of(p.x - v.x, p.y - v.y);
        return look;
    }

    std::vector<RouteStep> steps_;
    double start_s_;
    WalkRoom room_;
    Mouse mouse_;
    Turner turns_;
    State state_ = State::Stand;
    size_t next_ = 0, spot_ = 0, seg_ = 0, arrived_ = 0;
    std::vector<std::pair<double, double>> leg_;
    std::vector<double> cum_;
    double done_ = 0.0;
    std::string look_ = "-";
    double stand_until_ = 0, stopped_at_ = -10.0, pressed_at_ = 0, released_at_ = 0, clear_since_ = -1.0;
    bool held_ = false, have_last_ = false;
    double speed_ = 0, last_x_ = 0, last_y_ = 0, last_heading_ = 0, start_turn_ = 0;
    int missed_ = 0, held_back_ = 0;
    Keys leg_keys_ = keys_for(0.0);   // the leg's keys, and their way off the camera
    double leg_off_ = 0.0;
    std::vector<Keys> legs_keys_, taps_keys_;
    StandCam stand_cam_;
    bool walk_turn_ = false, stepping_ = false;   // the walk turn style; this leg is a step
    bool straight_ = false;                       // walk ... straight: legs set off the way the body faces
    double cam_rate_ = kTurnRate;                 // deg/s the camera turns at most, walking
    Keys tap_keys_;
    double tap_cam_ = 0.0, aim_since_ = 0.0;      // a tap's camera; aimed since
    bool tap_then_aim_ = false;                   // the tap turns the body toward a leg, not to look
    bool tap_more_ = false;                       // ... and is not the last toward this look
    int taps_ = 0, step_taps_ = 0;                // taps in all; toward the look being turned to
};

}  // namespace diogenes::gta::stage::robot
