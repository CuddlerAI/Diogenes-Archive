#include "stage_plan.hpp"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <limits>
#include <sstream>

namespace diogenes::gta::stage {

namespace {

constexpr double kPi = 3.14159265358979323846;
constexpr double kSampleM = 0.02;

double rad(double deg) { return deg * kPi / 180.0; }

// Rotate an offset measured at scene heading 0 into a scene at heading h.
void rotate(double h_deg, double x, double y, double& ox, double& oy) {
    const double c = std::cos(rad(h_deg)), s = std::sin(rad(h_deg));
    ox = x * c - y * s;
    oy = x * s + y * c;
}

std::string fmt(double v, int prec = 6) {
    char b[64];
    std::snprintf(b, sizeof(b), "%.*f", prec, v);
    return b;
}

bool to_double(const std::string& s, double& v) {
    if (s.empty()) return false;
    char* end = nullptr;
    v = std::strtod(s.c_str(), &end);
    return end && *end == '\0' && std::isfinite(v);
}
bool to_int(const std::string& s, int& v) {
    double d = 0;
    if (!to_double(s, d) || d != std::floor(d) || std::fabs(d) > 1e9) return false;
    v = static_cast<int>(d);
    return true;
}

}  // namespace

// ------------------------------------------------------------------ geometry

double norm_heading(double h) {
    h = std::fmod(h, 360.0);
    return h < 0 ? h + 360.0 : h;
}
double world_dir_to_heading(double dx, double dy) {
    return norm_heading(std::atan2(-dx, dy) * 180.0 / kPi);
}
double heading_delta(double a, double b) {
    return std::fabs(std::fmod(a - b + 540.0, 360.0) - 180.0);
}

void StageFrame::to_world(double xs, double ys, double& wx, double& wy) const {
    const double h = rad(heading_deg);
    wx = x + std::cos(h) * xs - std::sin(h) * ys;
    wy = y + std::sin(h) * xs + std::cos(h) * ys;
}
double StageFrame::heading_to_world(double stage_heading_deg) const {
    return norm_heading(heading_deg + stage_heading_deg);
}

void Path::at(double d, double& ox, double& oy, double& oz, double& heading_deg) const {
    if (s.size() < 2) { ox = x.empty() ? 0 : x[0]; oy = y.empty() ? 0 : y[0]; oz = z.empty() ? 0 : z[0]; heading_deg = 0; return; }
    const double L = total();
    if (loop) { d = std::fmod(d, L); if (d < 0) d += L; }
    // Past either end the body keeps going straight along the end tangent: the
    // planner may stretch a walk by part of a step so it stops feet-together.
    if (!loop && (d < 0 || d > L)) {
        const size_t i0 = d < 0 ? 0 : s.size() - 2;
        const double dx = x[i0 + 1] - x[i0], dy = y[i0 + 1] - y[i0];
        const double len = std::max(1e-9, s[i0 + 1] - s[i0]);
        const double from = d < 0 ? d : d - L;
        const size_t base = d < 0 ? 0 : s.size() - 1;
        ox = x[base] + dx / len * from;
        oy = y[base] + dy / len * from;
        oz = z[base];
        heading_deg = world_dir_to_heading(dx, dy);
        return;
    }
    const auto it = std::upper_bound(s.begin(), s.end(), d);
    const size_t i = std::min(s.size() - 1, static_cast<size_t>(std::max<std::ptrdiff_t>(1, it - s.begin())));
    const double t = std::max(0.0, std::min(1.0, (d - s[i - 1]) / std::max(1e-9, s[i] - s[i - 1])));
    ox = x[i - 1] + t * (x[i] - x[i - 1]);
    oy = y[i - 1] + t * (y[i] - y[i - 1]);
    oz = z[i - 1] + t * (z[i] - z[i - 1]);
    heading_deg = world_dir_to_heading(x[i] - x[i - 1], y[i] - y[i - 1]);
}
void Path::translate(double dx, double dy) {
    for (auto& v : x) v += dx;
    for (auto& v : y) v += dy;
}

PathBuilder::PathBuilder(double x0, double y0, double stage_heading_deg)
    : x(x0), y(y0), th(rad(stage_heading_deg + 90.0)) {
    px.push_back(x); py.push_back(y);
}
void PathBuilder::line(double len) {
    const int n = std::max(1, static_cast<int>(std::ceil(len / kSampleM)));
    const double dx = std::cos(th) * len / n, dy = std::sin(th) * len / n;
    for (int i = 0; i < n; ++i) { x += dx; y += dy; px.push_back(x); py.push_back(y); }
}
void PathBuilder::arc(double r, double dth_deg) {
    const double dth = rad(dth_deg);
    const double side = dth > 0 ? 1.0 : -1.0;
    const double cx = x - side * r * std::sin(th), cy = y + side * r * std::cos(th);
    const double phi0 = std::atan2(y - cy, x - cx);
    const int n = std::max(2, static_cast<int>(std::ceil(std::fabs(dth) * r / kSampleM)));
    for (int i = 1; i <= n; ++i) {
        const double phi = phi0 + dth * i / n;
        x = cx + r * std::cos(phi); y = cy + r * std::sin(phi);
        px.push_back(x); py.push_back(y);
    }
    th += dth;
}

// The ground is probed every 16 cm and filled in between: the stage is flat (the
// requirement's 10 x 10 m ground), this only follows gentle slopes.
void probe_ground(Path& p, const std::function<double(double, double)>& ground) {
    const size_t n = p.x.size();
    p.z.assign(n, 0.0);
    std::vector<size_t> probed;
    for (size_t i = 0; i < n; ++i)
        if (i % 8 == 0 || i + 1 == n) { p.z[i] = ground(p.x[i], p.y[i]); probed.push_back(i); }
    for (size_t k = 1; k < probed.size(); ++k) {
        const size_t a = probed[k - 1], c = probed[k];
        for (size_t j = a + 1; j < c; ++j) p.z[j] = p.z[a] + (p.z[c] - p.z[a]) * double(j - a) / double(c - a);
    }
}

Path make_path(const StageFrame& frame, const PathBuilder& b, bool loop,
               const std::function<double(double, double)>& ground) {
    Path p;
    p.loop = loop;
    double acc = 0;
    for (size_t i = 0; i < b.px.size(); ++i) {
        double wx = 0, wy = 0;
        frame.to_world(b.px[i], b.py[i], wx, wy);
        if (i > 0) acc += std::hypot(wx - p.x.back(), wy - p.y.back());
        p.s.push_back(acc); p.x.push_back(wx); p.y.push_back(wy);
    }
    probe_ground(p, ground);
    return p;
}

// ------------------------------------------------------------------ the spec

const ClipSpec* StageSpec::clip(const std::string& id) const {
    for (const auto& c : clips) if (c.id == id) return &c;
    return nullptr;
}
const PairSpec* StageSpec::pair(const std::string& id) const {
    for (const auto& p : pairs) if (p.id == id) return &p;
    return nullptr;
}
const PathSpec* StageSpec::path(const std::string& id) const {
    for (const auto& p : paths) if (p.id == id) return &p;
    return nullptr;
}
const RobotSpec* StageSpec::robot(const std::string& take) const {
    for (const auto& r : robots) if (r.take == take) return &r;
    return nullptr;
}
const SetSpec* StageSpec::set(const std::string& id) const {
    for (const auto& s : sets) if (s.id == id) return &s;
    return nullptr;
}
const TrackSpec* StageSpec::track(const std::string& id) const {
    for (const auto& t : tracks) if (t.id == id) return &t;
    return nullptr;
}
static double lift_of(const std::vector<LiftSpec>& lifts, const std::string& clip) {
    for (const auto& l : lifts) if (l.clip == clip) return l.metres;
    return 0.0;
}
double StageSpec::lift(const std::string& clip) const { return lift_of(lifts, clip); }
double Plan::lift(const std::string& clip) const { return lift_of(lifts, clip); }
const SeatSpec* StageSpec::seat(const std::string& actor) const {
    for (const auto& s : seats) if (s.actor == actor) return &s;
    return nullptr;
}
const ClipCal* Calibration::clip(const std::string& id) const {
    for (const auto& c : clips) if (c.id == id) return &c;
    return nullptr;
}
const SeatCal* Calibration::seat(const std::string& actor) const {
    for (const auto& s : seats) if (s.actor == actor) return &s;
    return nullptr;
}

std::string parse_stage_spec(const std::string& text, StageSpec& out) {
    out = StageSpec{};
    std::istringstream in(text);
    std::string line;
    int no = 0;
    bool header = false, have_frame = false;
    auto actor_of = [&](const std::string& id) -> ActorSpec* {
        for (auto& a : out.actors) if (a.id == id) return &a;
        return nullptr;
    };
    auto path_of = [&](const std::string& id) -> PathSpec* {
        for (auto& p : out.paths) if (p.id == id) return &p;
        return nullptr;
    };
    while (std::getline(in, line)) {
        ++no;
        if (!line.empty() && line.back() == '\r') line.pop_back();
        const std::string raw = line;   // a rule's say text keeps any '#' it holds
        const auto hash = line.find('#');
        if (hash != std::string::npos) line.erase(hash);
        std::istringstream ls(line);
        std::vector<std::string> t;
        for (std::string w; ls >> w;) t.push_back(w);
        if (t.empty()) continue;
        const std::string where = "line " + std::to_string(no) + ": ";
        if (!header) {
            if (t.size() != 1 || t[0] != "dm-gta-stage-spec/1") return where + "expected dm-gta-stage-spec/1";
            header = true;
            continue;
        }
        const std::string& cmd = t[0];
        auto need = [&](size_t n) { return t.size() == n; };
        if (cmd == "frame") {
            if (!need(5) || !to_double(t[1], out.frame.x) || !to_double(t[2], out.frame.y) || !to_double(t[3], out.frame.z) ||
                !to_double(t[4], out.frame.heading_deg))
                return where + "frame <x> <y> <ground z> <heading>";
            have_frame = true;
        } else if (cmd == "ticks") {
            if (!need(2) || !to_int(t[1], out.ticks) || out.ticks < 30 || out.ticks > 30 * 600) return where + "ticks <30..18000>";
        } else if (cmd == "clip") {
            if (!need(5)) return where + "clip <id> <dict> <clip> <loop|once|walk>";
            if (out.clip(t[1])) return where + "duplicate clip " + t[1];
            ClipSpec c{t[1], t[2], t[3], ClipKind::Loop};
            if (t[4] == "loop") c.kind = ClipKind::Loop;
            else if (t[4] == "once") c.kind = ClipKind::Once;
            else if (t[4] == "walk") c.kind = ClipKind::Walk;
            else return where + "clip kind must be loop, once or walk";
            out.clips.push_back(c);
        } else if (cmd == "pair") {
            PairSpec p;
            if (!need(7) || !to_double(t[4], p.x) || !to_double(t[5], p.y) || !to_double(t[6], p.heading))
                return where + "pair <id> <clip_a> <clip_b> <x> <y> <heading>";
            p.id = t[1]; p.clip_a = t[2]; p.clip_b = t[3];
            const ClipSpec* a = out.clip(p.clip_a);
            const ClipSpec* b = out.clip(p.clip_b);
            if (!a || !b || a->kind != ClipKind::Once || b->kind != ClipKind::Once) return where + "a pair's clips must be declared once-clips";
            if (out.pair(p.id)) return where + "duplicate pair " + p.id;
            out.pairs.push_back(p);
        } else if (cmd == "actor") {
            if (!need(3)) return where + "actor <id> <model>";
            if (actor_of(t[1])) return where + "duplicate actor " + t[1];
            ActorSpec a; a.id = t[1]; a.model = t[2];
            out.actors.push_back(a);
        } else if (cmd == "look") {
            LookSpec l;
            ActorSpec* a = need(5) ? actor_of(t[1]) : nullptr;
            if (!a || !to_int(t[2], l.component) || !to_int(t[3], l.drawable) || !to_int(t[4], l.texture) ||
                l.component < 0 || l.component > 11 || l.drawable < 0 || l.texture < 0)
                return where + "look <actor> <component 0..11> <drawable> <texture>";
            a->looks.push_back(l);
        } else if (cmd == "prop") {
            PropSpec p;
            ActorSpec* a = need(4) ? actor_of(t[1]) : nullptr;
            if (!a || !to_int(t[3], p.bone) || p.bone <= 0) return where + "prop <actor> <model> <bone id>";
            p.model = t[2];
            a->props.push_back(p);
        } else if (cmd == "propshow") {
            PropShow s;
            ActorSpec* a = need(5) ? actor_of(t[1]) : nullptr;
            if (!a || !to_double(t[3], s.from) || !to_double(t[4], s.to) || !(s.from >= 0.0 && s.from < s.to && s.to <= 1.0))
                return where + "propshow <actor> <clip> <from 0..1> <to 0..1, above from>";
            if (!out.clip(t[2])) return where + "propshow: clip " + t[2] + " is not declared";
            s.clip = t[2];
            a->prop_shows.push_back(s);
        } else if (cmd == "path") {
            PathSpec p;
            int loop = 0;
            if (!need(6) || !to_int(t[2], loop) || (loop != 0 && loop != 1) || !to_double(t[3], p.x0) ||
                !to_double(t[4], p.y0) || !to_double(t[5], p.heading))
                return where + "path <id> <loop 0|1> <x0> <y0> <heading>";
            if (path_of(t[1])) return where + "duplicate path " + t[1];
            p.id = t[1]; p.loop = loop == 1;
            out.paths.push_back(p);
        } else if (cmd == "line" || cmd == "arc") {
            PathSpec* p = t.size() >= 2 ? path_of(t[1]) : nullptr;
            PathSpec::Seg g;
            g.arc = cmd == "arc";
            const bool ok = p && (g.arc ? need(4) && to_double(t[2], g.a) && to_double(t[3], g.b) && g.a > 0.05 &&
                                              std::fabs(g.b) >= 1.0 && std::fabs(g.b) <= 360
                                        : need(3) && to_double(t[2], g.a) && g.a > 0);
            if (!ok) return where + (g.arc ? "arc <path> <radius > 0.05> <degrees, 1..360 either way>" : "line <path> <length > 0>");
            p->segs.push_back(g);
        } else if (cmd == "beat") {
            ActorSpec* a = t.size() >= 3 ? actor_of(t[1]) : nullptr;
            if (!a) return where + "beat <actor> <kind> ...: unknown actor";
            BeatSpec b;
            const std::string& kind = t[2];
            size_t at = 0;
            if (kind == "idle") {
                const ClipSpec* c = t.size() >= 5 ? out.clip(t[3]) : nullptr;
                if (!c || c->kind != ClipKind::Loop || !to_double(t[4], b.seconds) || b.seconds <= 0)
                    return where + "beat <actor> idle <loop clip> <seconds> [at x y heading]";
                b.kind = BeatKind::Idle; b.clip = t[3]; at = 5;
            } else if (kind == "walk") {
                const ClipSpec* c = t.size() >= 5 ? out.clip(t[3]) : nullptr;
                if (!c || c->kind != ClipKind::Walk || !out.path(t[4]) || t.size() != 5)
                    return where + "beat <actor> walk <walk clip> <path>";
                b.kind = BeatKind::Walk; b.clip = t[3]; b.path = t[4]; at = 5;
            } else if (kind == "once") {
                const ClipSpec* c = t.size() >= 4 ? out.clip(t[3]) : nullptr;
                if (!c || c->kind != ClipKind::Once) return where + "beat <actor> once <once clip> [at x y heading]";
                b.kind = BeatKind::Once; b.clip = t[3]; at = 4;
            } else if (kind == "pair") {
                if (t.size() != 5 || !out.pair(t[3]) || (t[4] != "a" && t[4] != "b")) return where + "beat <actor> pair <pair> <a|b>";
                b.kind = BeatKind::Pair; b.pair = t[3]; b.pair_role = t[4][0]; at = 5;
            } else {
                return where + "beat kind must be idle, walk, once or pair";
            }
            if (t.size() > at) {
                if (t.size() != at + 4 || t[at] != "at" || !to_double(t[at + 1], b.at_x) || !to_double(t[at + 2], b.at_y) ||
                    !to_double(t[at + 3], b.at_heading))
                    return where + "trailing words; only \"at <x> <y> <heading>\" may follow";
                if (!a->beats.empty()) return where + "\"at\" places an actor's first beat only; later beats are placed by the plan";
                b.has_at = true;
            }
            a->beats.push_back(b);
        } else if (cmd == "transition") {
            // transition <snap|blend> [speed]            the stage's way to change clip
            // transition <actor> <snap|blend> [speed]    one actor's
            size_t i = 1;
            ActorSpec* who = nullptr;
            if (t.size() >= 2 && t[1] != "snap" && t[1] != "blend") {
                who = actor_of(t[1]);
                if (!who) return where + "transition: unknown actor " + t[1];
                i = 2;
            }
            if (t.size() < i + 1 || t.size() > i + 2 || (t[i] != "snap" && t[i] != "blend"))
                return where + "transition [<actor>] <snap|blend> [blend-in speed]";
            double v = 0;
            if (t[i] == "blend") {
                // A blend lasts 1/speed seconds. The runtime retires the old scene 15
                // ticks after the change and the gate forgives the pose 20 ticks, so
                // nothing slower than a quarter second (measured, rehearsal v6) fits.
                v = 4.0;
                if (t.size() == i + 2 && (!to_double(t[i + 1], v) || v < 4.0 || v > 1000.0))
                    return where + "transition: blend-in speed must be in [4, 1000] (at most a quarter second)";
            } else if (t.size() == i + 2) {
                return where + "transition: a snap takes no speed";
            }
            if (who) who->blend_in = v;
            else out.blend_in = v;
        } else if (cmd == "rule") {
            // rule <id> <when> <actor> [metres] then <actor>:<action>[,...] say <text...>
            RuleSpec r;
            size_t i = 1;
            if (t.size() < 7) return where + "rule <id> <near|touch|aim|damage> <actor> [metres] then <actor>:<action>[,...] say <text>";
            r.id = t[i++];
            r.when = t[i++];
            if (r.when != "near" && r.when != "touch" && r.when != "aim" && r.when != "damage") return where + "rule condition must be near, touch, aim or damage";
            r.subject = t[i++];
            if (!actor_of(r.subject)) return where + "rule " + r.id + ": unknown actor " + r.subject;
            if (r.when == "near") {
                if (i >= t.size() || !to_double(t[i], r.metres) || r.metres <= 0 || r.metres > 20) return where + "rule " + r.id + ": near needs metres in (0, 20]";
                ++i;
            }
            if (i >= t.size() || t[i] != "then") return where + "rule " + r.id + ": expected then";
            ++i;
            if (i >= t.size()) return where + "rule " + r.id + ": nothing after then";
            std::istringstream acts(t[i++]);
            for (std::string a; std::getline(acts, a, ',');) {
                const auto colon = a.find(':');
                if (colon == std::string::npos) return where + "rule " + r.id + ": reactions are <actor>:<action>";
                const std::string who = a.substr(0, colon), what = a.substr(colon + 1);
                if (!actor_of(who)) return where + "rule " + r.id + ": unknown actor " + who;
                if (what != "flee" && what != "handsup" && what != "cower" && what != "fight" && what != "watch")
                    return where + "rule " + r.id + ": action must be flee, handsup, cower, fight or watch";
                r.then.push_back({who, what});
            }
            if (i >= t.size() || t[i] != "say") return where + "rule " + r.id + ": expected say <text> -- every rule carries its causal record";
            // The text runs to the end of the raw line, spaces and '#' and all. Its
            // start is found by counting words: no '#' comes before the say word, or
            // the words before it would not have parsed.
            size_t pos = 0, end = 0;
            for (size_t w = 0; w <= i; ++w) {
                pos = raw.find_first_not_of(" \t", end);
                end = pos == std::string::npos ? std::string::npos : raw.find_first_of(" \t", pos);
                if (end == std::string::npos) break;
            }
            const size_t from = end == std::string::npos ? std::string::npos : raw.find_first_not_of(" \t", end);
            r.say = from == std::string::npos ? std::string() : raw.substr(from);
            while (!r.say.empty() && (r.say.back() == ' ' || r.say.back() == '\t')) r.say.pop_back();
            if (r.say.empty()) return where + "rule " + r.id + ": say needs text";
            for (const auto& q : out.rules) if (q.id == r.id) return where + "duplicate rule " + r.id;
            out.rules.push_back(r);
        } else if (cmd == "robot") {
            RobotSpec r;
            double seed = -1;
            if (!need(4) || !to_double(t[3], seed) || seed < 0 || seed >= 4294967296.0 || seed != std::floor(seed))
                return where + "robot <AB|C|D> <fly|changed|unchanged> <seed>";
            r.take = t[1];
            r.mode = t[2];
            r.seed = static_cast<std::uint32_t>(seed);
            const bool walker = r.take == "C" || r.take == "D";
            if (r.take == "AB" ? r.mode != "fly" : !walker || (r.mode != "changed" && r.mode != "unchanged"))
                return where + "robot: AB flies (fly); C and D walk (changed or unchanged)";
            if (out.robot(r.take)) return where + "duplicate robot for " + r.take;
            out.robots.push_back(r);
        } else if (cmd == "sink") {
            if (!need(2) || !to_double(t[1], out.sink) || out.sink < 0.0 || out.sink > 0.1) return where + "sink <metres, 0..0.1>";
        } else if (cmd == "contact") {
            ContactSpec c;
            if (!need(3)) return where + "contact <clip|*> <frame|const|off>";
            c.clip = t[1];
            if (t[2] == "frame") c.mode = ContactMode::Frame;
            else if (t[2] == "const") c.mode = ContactMode::Const;
            else if (t[2] == "off") c.mode = ContactMode::Off;
            else return where + "contact <clip|*> <frame|const|off>";
            for (const auto& o : out.contacts) if (o.clip == c.clip) return where + "duplicate contact for " + c.clip;
            out.contacts.push_back(c);
        } else if (cmd == "stance") {
            if (need(2) && (t[1] == "game" || t[1] == "calm")) {
                out.stance = t[1];
                out.stance_dict.clear();
                out.stance_clip.clear();
            } else if (need(4) && t[1] == "clip") {
                out.stance = "clip";
                out.stance_dict = t[2];
                out.stance_clip = t[3];
            } else {
                return where + "stance game|calm|clip <dict> <clip>";
            }
        } else if (cmd == "legik") {
            if (t.size() < 2 || t.size() > 3 || !to_int(t[1], out.leg_ik) || out.leg_ik < 0 || out.leg_ik > 3 ||
                (t.size() == 3 && (!to_int(t[2], out.scene_ik_flags) || out.scene_ik_flags < 0 || out.scene_ik_flags > 65535)))
                return where + "legik <0..3> [scene IK flags 0..65535]";
        } else if (cmd == "lift") {
            LiftSpec l;
            if (!need(3) || !to_double(t[2], l.metres) || !(l.metres >= -0.1 && l.metres <= 0.1)) return where + "lift <clip> <metres, -0.1..0.1>";
            if (!out.clip(t[1])) return where + "lift: clip " + t[1] + " is not declared";
            for (const auto& had : out.lifts) if (had.clip == t[1]) return where + "duplicate lift for " + t[1];
            l.clip = t[1];
            out.lifts.push_back(l);
        } else if (cmd == "set") {
            SetSpec s;
            if (!need(6) || !to_double(t[3], s.x) || !to_double(t[4], s.y) || !to_double(t[5], s.heading))
                return where + "set <id> <model>[|<model>...] <x> <y> <heading>";
            s.id = t[1];
            std::istringstream models(t[2]);
            for (std::string m; std::getline(models, m, '|');) if (!m.empty()) s.models.push_back(m);
            if (s.models.empty()) return where + "set " + s.id + " names no model";
            if (out.set(s.id)) return where + "duplicate set " + s.id;
            out.sets.push_back(s);
        } else if (cmd == "track") {
            TrackSpec k;
            if (!(need(5) || need(6)) || !to_double(t[3], k.x) || !to_double(t[4], k.y) ||
                (t.size() == 6 && (!to_double(t[5], k.radius) || !(k.radius >= 0.5 && k.radius <= 200.0))))
                return where + "track <id> <model or 0x<hash>> <x> <y> [<radius, 0.5..200 m>]";
            k.id = t[1];
            k.model = t[2];
            if (out.track(k.id) || out.set(k.id)) return where + "duplicate track " + k.id;
            for (const auto& c : out.clears) if (c.model == k.model) return where + "track " + k.id + ": the spec clears " + c.model;
            out.tracks.push_back(k);
        } else if (cmd == "clear") {
            ClearSpec c;
            if (!need(5) || !to_double(t[2], c.x) || !to_double(t[3], c.y) || !to_double(t[4], c.radius) ||
                !(c.radius >= 1.0 && c.radius <= 300.0))
                return where + "clear <model or 0x<hash>> <x> <y> <radius, 1..300 m>";
            c.model = t[1];
            for (const auto& had : out.tracks) if (had.model == c.model) return where + "clear " + c.model + ": the spec tracks it (" + had.id + ")";
            out.clears.push_back(c);
        } else if (cmd == "seat") {
            const ClipSpec* c = need(4) ? out.clip(t[3]) : nullptr;
            if (!c || c->kind != ClipKind::Loop || !actor_of(t[1]) || !out.set(t[2]))
                return where + "seat <actor> <set> <loop clip>: the actor, the set piece and the clip declared before";
            if (out.seat(t[1])) return where + "actor " + t[1] + " has a seat already";
            out.seats.push_back({t[1], t[2], t[3]});
        } else if (cmd == "repeat") {
            ActorSpec* a = need(3) ? actor_of(t[1]) : nullptr;
            int from = -1;
            if (!a || !to_int(t[2], from) || from < 0 || from >= static_cast<int>(a->beats.size()))
                return where + "repeat <actor> <index of an existing beat>";
            a->repeat_from = from;
        } else {
            return where + "unknown command " + cmd;
        }
    }
    if (!header) return "empty spec";
    if (!have_frame) return "no frame";
    if (out.ticks <= 0) return "no ticks";
    for (const auto& p : out.paths) if (p.segs.empty()) return "path " + p.id + " has no segments";
    for (const auto& a : out.actors) {
        if (a.beats.empty()) return "actor " + a.id + " has no beats";
        const BeatSpec& first = a.beats[0];
        if (first.kind == BeatKind::Walk) continue;   // a walk starts where its path starts
        if (first.kind == BeatKind::Pair) continue;   // a pair places its actors itself
        if (first.has_at) continue;
        // an idle before a pair may take its place from the pair
        bool pair_next = a.beats.size() > 1 && a.beats[1].kind == BeatKind::Pair;
        if (!(first.kind == BeatKind::Idle && pair_next)) return "actor " + a.id + ": its first beat needs \"at x y heading\"";
    }
    return "";
}

// ------------------------------------------------------------------ calibration

double pose_distance(const ClipCal& a, int ja, const ClipCal& b, int jb) {
    if (a.n <= 0 || b.n <= 0 || a.pose.size() != static_cast<size_t>(a.n) * kPoseBones * 3 ||
        b.pose.size() != static_cast<size_t>(b.n) * kPoseBones * 3) return 1e9;
    ja = ((ja % a.n) + a.n) % a.n;
    jb = ((jb % b.n) + b.n) % b.n;
    double s = 0;
    for (int k = 0; k < kPoseBones; ++k) {
        const double dx = a.pose[a.pose_at(ja, k, 0)] - b.pose[b.pose_at(jb, k, 0)];
        const double dy = a.pose[a.pose_at(ja, k, 1)] - b.pose[b.pose_at(jb, k, 1)];
        const double dz = a.pose[a.pose_at(ja, k, 2)] - b.pose[b.pose_at(jb, k, 2)];
        s += std::sqrt(dx * dx + dy * dy + dz * dz);
    }
    return s;
}

namespace {
std::string exact(double v) {
    char b[40];
    std::snprintf(b, sizeof(b), "%.17g", v);
    return b;
}
}  // namespace

ContactMode Plan::contact_mode(const std::string& clip, bool walk) const {
    ContactMode m = ContactMode::Default;
    for (const auto& c : contacts) if (c.clip == clip) m = c.mode;
    if (m == ContactMode::Default) for (const auto& c : contacts) if (c.clip == "*") m = c.mode;
    if (m == ContactMode::Default) m = walk ? ContactMode::Const : ContactMode::Frame;
    return m;
}
double Plan::contact_correction(const ClipCal& cc, int j, bool walk) const {
    switch (contact_mode(cc.id, walk)) {
        case ContactMode::Frame: return cc.contact_at(j);
        case ContactMode::Const: return cc.contact_min();
        default: return 0.0;
    }
}

std::string render_calibration(const Calibration& cal) {
    std::ostringstream o;
    o << "dm-gta-stage-calibration/1\n";
    for (const auto& c : cal.clips) {
        o << "clip " << c.id << " n " << c.n << " duration " << exact(c.duration_s) << " zoff " << exact(c.zoff)
          << " speed " << exact(c.speed) << "\n";
        for (int j = 0; j < c.n; ++j) {
            o << "  " << j << " " << exact(c.root_x[static_cast<size_t>(j)]) << " " << exact(c.root_y[static_cast<size_t>(j)])
              << " " << exact(c.yaw[static_cast<size_t>(j)]);
            for (int k = 0; k < kPoseBones * 3; ++k) o << " " << exact(c.pose[static_cast<size_t>(j * kPoseBones * 3 + k)]);
            if (!c.contact.empty()) o << " " << exact(c.contact[static_cast<size_t>(j)]);
            o << "\n";
        }
    }
    for (const auto& s : cal.seats)
        o << "seat " << s.actor << " found " << (s.found ? 1 : 0) << " x " << exact(s.x) << " y " << exact(s.y) << " z " << exact(s.z)
          << " heading " << exact(s.heading) << "\n";
    o << "end\n";
    return o.str();
}

std::string parse_calibration(const std::string& text, Calibration& out) {
    out = Calibration{};
    std::istringstream in(text);
    std::string line;
    int no = 0;
    bool header = false, ended = false;
    ClipCal* cur = nullptr;
    int expect = 0;
    while (std::getline(in, line)) {
        ++no;
        if (!line.empty() && line.back() == '\r') line.pop_back();
        std::istringstream ls(line);
        std::vector<std::string> t;
        for (std::string w; ls >> w;) t.push_back(w);
        if (t.empty()) continue;
        const std::string where = "line " + std::to_string(no) + ": ";
        if (ended) return where + "text after end";
        if (!header) {
            if (t.size() != 1 || t[0] != "dm-gta-stage-calibration/1") return where + "expected dm-gta-stage-calibration/1";
            header = true;
            continue;
        }
        if (t[0] == "end" && t.size() == 1) {
            if (cur && expect != cur->n) return where + "clip " + cur->id + " is missing phases";
            ended = true;
            continue;
        }
        if (t[0] == "clip") {
            if (cur && expect != cur->n) return where + "clip " + cur->id + " is missing phases";
            ClipCal c;
            if (t.size() != 10 || t[2] != "n" || t[4] != "duration" || t[6] != "zoff" || t[8] != "speed" || !to_int(t[3], c.n) ||
                c.n < 2 || c.n > 100000 || !to_double(t[5], c.duration_s) || !to_double(t[7], c.zoff) || !to_double(t[9], c.speed))
                return where + "clip <id> n <n> duration <s> zoff <m> speed <m/s>";
            if (out.clip(t[1])) return where + "duplicate clip " + t[1];
            c.id = t[1];
            out.clips.push_back(c);
            cur = &out.clips.back();
            expect = 0;
            continue;
        }
        if (t[0] == "seat") {
            if (cur && expect != cur->n) return where + "clip " + cur->id + " is missing phases";
            SeatCal s;
            int found = -1;
            if (t.size() != 12 || t[2] != "found" || t[4] != "x" || t[6] != "y" || t[8] != "z" || t[10] != "heading" ||
                !to_int(t[3], found) || (found != 0 && found != 1) || !to_double(t[5], s.x) || !to_double(t[7], s.y) ||
                !to_double(t[9], s.z) || !to_double(t[11], s.heading))
                return where + "seat <actor> found <0|1> x <m> y <m> z <m> heading <deg>";
            if (out.seat(t[1])) return where + "duplicate seat " + t[1];
            s.actor = t[1];
            s.found = found == 1;
            out.seats.push_back(s);
            cur = nullptr;   // no phases follow a seat
            continue;
        }
        int j = -1;
        const bool with_contact = t.size() == static_cast<size_t>(5 + kPoseBones * 3);
        if (!cur || (t.size() != static_cast<size_t>(4 + kPoseBones * 3) && !with_contact) || !to_int(t[0], j) || j != expect || j >= cur->n)
            return where + "expected phase " + std::to_string(expect) + " of the current clip";
        if (j > 0 && with_contact != (cur->contact.size() == static_cast<size_t>(j)))
            return where + "clip " + cur->id + ": a contact on some phases only";
        double v = 0;
        if (!to_double(t[1], v)) return where + "bad number"; cur->root_x.push_back(v);
        if (!to_double(t[2], v)) return where + "bad number"; cur->root_y.push_back(v);
        if (!to_double(t[3], v)) return where + "bad number"; cur->yaw.push_back(v);
        for (int k = 0; k < kPoseBones * 3; ++k) {
            if (!to_double(t[static_cast<size_t>(4 + k)], v)) return where + "bad number";
            cur->pose.push_back(v);
        }
        if (with_contact) {
            if (!to_double(t.back(), v) || v < -0.5 || v > 2.0) return where + "bad contact";
            cur->contact.push_back(v);
        }
        ++expect;
    }
    if (!header) return "empty calibration";
    if (!ended) return "no end line: the file is cut short";
    return "";
}

// ------------------------------------------------------------------ the plan

namespace {

// Where the body is, which way it faces and what pose it holds at the end of a beat.
struct BodyState {
    bool valid = false;
    double x = 0, y = 0, heading = 0;
    const ClipCal* clip = nullptr;
    int j = 0;
    bool once = false;   // its clip plays once: it holds its last pose instead of going round
};

// The pose the body would show on the next tick had its clip gone on. A new
// beat's first tick should look like that, not like the last one again -- the
// same clip twice in a row then simply carries on.
int next_pose(const BodyState& body) {
    return body.once ? std::min(body.clip->n - 1, body.j + 1) : (body.j + 1) % body.clip->n;
}

// The clip an actor plays in a beat (its own half, for a pair).
std::string beat_clip(const StageSpec& spec, const BeatSpec& b) {
    if (b.kind != BeatKind::Pair) return b.clip;
    const PairSpec* p = spec.pair(b.pair);
    return b.pair_role == 'a' ? p->clip_a : p->clip_b;
}

// Can a beat choose the phase it starts on? Loops and walks can; a clip played
// once starts at its beginning.
bool free_entry(const BeatSpec& b) { return b.kind == BeatKind::Idle || b.kind == BeatKind::Walk; }

// The entry phase of `next` that best continues from pose (from, j).
int best_entry(const ClipCal& from, int j, const ClipCal& next, bool free, double* cost) {
    if (!free) { if (cost) *cost = pose_distance(from, j, next, 0); return 0; }
    int best = 0; double bc = 1e18;
    for (int k = 0; k < next.n; ++k) {
        const double c = pose_distance(from, j, next, k);
        if (c < bc - 1e-12) { bc = c; best = k; }
    }
    if (cost) *cost = bc;
    return best;
}

// The body on a pair's first tick: the pair's origin plus this half's own offset.
void pair_entry(const StageSpec& spec, const PairSpec& p, const ClipCal& half, double& x, double& y, double& h) {
    double ox = 0, oy = 0;
    spec.frame.to_world(p.x, p.y, ox, oy);
    const double ph = spec.frame.heading_to_world(p.heading);
    double rx = 0, ry = 0;
    rotate(ph, half.root_x[0], half.root_y[0], rx, ry);
    x = ox + rx; y = oy + ry;
    h = norm_heading(ph + half.yaw[0]);
}

bool cal_ok(const ClipCal* c) {
    return c && c->n >= 2 && c->root_x.size() == static_cast<size_t>(c->n) && c->root_y.size() == static_cast<size_t>(c->n) &&
           c->yaw.size() == static_cast<size_t>(c->n) && c->pose.size() == static_cast<size_t>(c->n) * kPoseBones * 3;
}

}  // namespace

std::string resolve_plan(const StageSpec& spec, const Calibration& cal,
                         const std::function<double(double, double)>& ground, Plan& out) {
    out = Plan{};
    out.frame = spec.frame;
    out.ticks = spec.ticks;
    out.sink = spec.sink;
    out.leg_ik = spec.leg_ik;
    out.contacts = spec.contacts;
    out.scene_ik_flags = spec.scene_ik_flags;
    out.lifts = spec.lifts;
    for (const auto& c : spec.clips) {
        if (!cal_ok(cal.clip(c.id))) return "clip " + c.id + " is not calibrated (or its tables are the wrong size)";
        if (c.kind == ClipKind::Walk && cal.clip(c.id)->speed < 0.3) return "walk clip " + c.id + " barely moves";
    }
    for (const auto& s : spec.sets) {   // on the ground where it stands, the same in every take
        SetPlacement p;
        p.id = s.id;
        spec.frame.to_world(s.x, s.y, p.x, p.y);
        p.z = ground(p.x, p.y);
        p.heading = spec.frame.heading_to_world(s.heading);
        out.sets.push_back(p);
    }
    for (const auto& a : spec.actors) {
        ActorPlan ap;
        ap.id = a.id;
        // A seat the game showed where a person sits (SeatCal): the seated clip plays there.
        const SeatSpec* seat = spec.seat(a.id);
        const SeatCal* seated = seat ? cal.seat(a.id) : nullptr;
        const SetPlacement* piece = nullptr;
        for (const auto& p : out.sets) if (seat && p.id == seat->set) piece = &p;
        if (seat && !(seated && seated->found))
            out.notes.push_back(a.id + ": the game seated nobody on " + seat->set + " -- the actor plays at its \"at\"");
        if (!(seated && seated->found)) seated = nullptr;
        BodyState body;
        int tick = 0;
        size_t bi = 0;
        while (tick < spec.ticks) {
            if (bi >= a.beats.size()) {
                if (a.repeat_from < 0) {   // nothing more to play: the last beat holds to the end
                    ap.beats.back().ticks += spec.ticks - tick;
                    break;
                }
                bi = static_cast<size_t>(a.repeat_from);
            }
            const BeatSpec& b = a.beats[bi];
            const BeatSpec* nb = nullptr;
            if (bi + 1 < a.beats.size()) nb = &a.beats[bi + 1];
            else if (a.repeat_from >= 0) nb = &a.beats[static_cast<size_t>(a.repeat_from)];
            const ClipCal& cc = *cal.clip(beat_clip(spec, b));
            const ClipCal* nc = nb ? cal.clip(beat_clip(spec, *nb)) : nullptr;

            ResolvedBeat r;
            r.kind = b.kind;
            r.clip = cc.id;
            r.start = tick;
            r.blend_in = a.blend_in >= 0 ? a.blend_in : spec.blend_in;
            r.cycle = cc.n;
            if (b.kind == BeatKind::Pair) {
                // one scene for both halves, so one phase: the longer half's cycle
                const PairSpec& p = *spec.pair(b.pair);
                r.cycle = std::max(cal.clip(p.clip_a)->n, cal.clip(p.clip_b)->n);
                r.pair_role = b.pair_role;
            }
            // an idle that waits for a pair keeps its length: both halves of the pair
            // must arrive on the same tick
            const bool waits = b.kind == BeatKind::Idle && nb && nb->kind == BeatKind::Pair;
            const bool entered = body.valid;   // a beat before this one left the body somewhere

            // ---- the entry: which phase, and where the body stands ----
            if (body.valid) r.phase0 = best_entry(*body.clip, next_pose(body), cc, free_entry(b), &r.entry_pop);
            double bx = 0, by = 0, bh = 0;   // the body on the beat's first tick
            if (b.kind == BeatKind::Pair) {
                const PairSpec& p = *spec.pair(b.pair);
                r.pair_index = static_cast<int>(spec.pair(b.pair) - &spec.pairs[0]);
                pair_entry(spec, p, cc, bx, by, bh);
                if (body.valid && std::hypot(bx - body.x, by - body.y) > 0.05)
                    out.notes.push_back(a.id + ": steps " + fmt(std::hypot(bx - body.x, by - body.y), 3) + " m into pair " + p.id);
            } else if (b.kind == BeatKind::Walk) {
                // walks start where the body is: the authored path is moved onto it
                const PathSpec& ps = *spec.path(b.path);
                PathBuilder pb(ps.x0, ps.y0, ps.heading);
                for (const auto& g : ps.segs) { if (g.arc) pb.arc(g.a, g.b); else pb.line(g.a); }
                Path path = make_path(spec.frame, pb, ps.loop, ground);
                if (body.valid) {
                    path.translate(body.x - path.x[0], body.y - path.y[0]);
                    probe_ground(path, ground);   // where the moved path now lies
                    double z = 0, h0 = 0, px = 0, py = 0;
                    path.at(0, px, py, z, h0);
                    if (heading_delta(h0, body.heading) > 10.0)
                        out.notes.push_back(a.id + ": path " + ps.id + " starts " + fmt(heading_delta(h0, body.heading), 1) +
                                            " deg off the way the body faces -- it will turn on the spot");
                }
                r.path = static_cast<int>(out.paths.size());
                out.paths.push_back(path);
                r.speed = cc.speed;
            } else {
                if (body.valid) { bx = body.x; by = body.y; bh = body.heading; }
                else if (b.has_at) {
                    spec.frame.to_world(b.at_x, b.at_y, bx, by);
                    bh = spec.frame.heading_to_world(b.at_heading);
                } else {   // an idle waiting for a pair stands where the pair will need it
                    const PairSpec& p = *spec.pair(nb->pair);
                    pair_entry(spec, p, *nc, bx, by, bh);
                }
            }

            // ---- the length: nominal, moved by part of a cycle so the exit pose
            // meets the next beat's entry pose as closely as the clips allow ----
            int nominal = 0;
            bool flexible = false;
            if (b.kind == BeatKind::Idle) {
                nominal = std::max(1, static_cast<int>(std::lround(b.seconds * kTickHz)));
                flexible = !waits;
            } else if (b.kind == BeatKind::Walk) {
                const double L = out.paths[static_cast<size_t>(r.path)].total();
                nominal = std::max(2, static_cast<int>(std::ceil(L / (cc.speed / kTickHz))));
                flexible = true;
            } else {
                nominal = r.cycle;   // once and pair clips play exactly once
            }
            r.ticks = nominal;
            if (waits) {
                // Its length is fixed, so the phase it starts on must serve both ends:
                // carry on from the pose it enters from (if any), and meet the pair's
                // first pose on its way out.
                double bc = 1e18;
                for (int j0 = 0; j0 < cc.n; ++j0) {
                    double c = pose_distance(cc, (j0 + nominal) % cc.n, *nc, 0);
                    if (entered) c += pose_distance(*body.clip, next_pose(body), cc, j0);
                    if (c < bc - 1e-12) { bc = c; r.phase0 = j0; }
                }
                if (entered) r.entry_pop = pose_distance(*body.clip, next_pose(body), cc, r.phase0);
            }
            if (flexible && nc) {
                // A walk passes a feet-together pose twice a cycle, so a quarter cycle
                // either way always reaches one; the stop moves by that much walking,
                // which is paid for so a small gain in pose does not buy a long detour.
                // An idle may run up to half a cycle longer or shorter, though never
                // shorter than half a cycle (or its own length, if that is less); a
                // walk's length is its path's, so only the quarter cycle bounds it.
                const bool walk = b.kind == BeatKind::Walk;
                const int span = walk ? std::max(1, cc.n / 4) : cc.n / 2;
                const int shortest = walk ? 2 : std::max(2, std::min(nominal, cc.n / 2));
                double bc = 1e18;
                int best = nominal;
                for (int d = -span; d <= span; ++d) {
                    const int T = nominal + d;
                    if (T < shortest) continue;
                    double c = 0;
                    best_entry(cc, (r.phase0 + T) % cc.n, *nc, free_entry(*nb), &c);
                    c += walk ? 0.5 * std::abs(d) * cc.speed / kTickHz : 1e-4 * std::abs(d);
                    if (c < bc - 1e-12) { bc = c; best = T; }
                }
                if (best != nominal)
                    out.notes.push_back(a.id + " beat " + std::to_string(ap.beats.size()) + " (" + cc.id + "): " + std::to_string(nominal) +
                                        " -> " + std::to_string(best) + " ticks to meet the next pose");
                r.ticks = best;
            }
            r.ticks = std::min(r.ticks, spec.ticks - tick);

            // ---- the scene: origin and heading, and where the body ends ----
            // the clip's own phase index on the beat's last tick
            const int j_last = b.kind == BeatKind::Once || b.kind == BeatKind::Pair
                                   ? std::min(cc.n - 1, static_cast<int>((r.ticks - 0.5) / r.cycle * cc.n))
                                   : (r.phase0 + r.ticks - 1) % cc.n;
            if (b.kind == BeatKind::Walk) {
                const Path& path = out.paths[static_cast<size_t>(r.path)];
                double z = 0;
                // The body stops where it was on the walk's last tick. Another walk
                // carries on instead: it starts half a tick on, so its first tick
                // lands one tick's walking further, like any other tick.
                const bool walks_on = nb && nb->kind == BeatKind::Walk;
                path.at(cc.speed * (walks_on ? r.ticks : r.ticks - 0.5) / kTickHz, body.x, body.y, z, body.heading);
                r.ground = z;
            } else {
                const bool on_seat = seated && piece && b.kind == BeatKind::Idle && b.clip == seat->clip;
                if (b.kind == BeatKind::Pair) {
                    const PairSpec& p = *spec.pair(b.pair);
                    spec.frame.to_world(p.x, p.y, r.ox, r.oy);
                    r.heading = spec.frame.heading_to_world(p.heading);
                } else if (on_seat) {
                    double sx = 0, sy = 0;
                    rotate(piece->heading, seated->x, seated->y, sx, sy);
                    r.ox = piece->x + sx; r.oy = piece->y + sy;
                    r.heading = norm_heading(piece->heading + seated->heading);
                } else {
                    // Origin such that the body stands exactly where it should, facing its
                    // way: on the beat's first tick -- except for an unplaced first beat
                    // that waits for a pair, which must end where the pair takes the body over.
                    const size_t j = static_cast<size_t>(waits && !entered && !b.has_at ? j_last : r.phase0);
                    r.heading = norm_heading(bh - cc.yaw[j]);
                    double rx = 0, ry = 0;
                    rotate(r.heading, cc.root_x[j], cc.root_y[j], rx, ry);
                    r.ox = bx - rx; r.oy = by - ry;
                }
                r.ground = ground(bx, by);
                r.oz = r.ground + cc.zoff - spec.sink + spec.lift(r.clip);
                if (b.kind == BeatKind::Pair) {
                    // one scene, one height, for two people whose own references differ
                    // by a centimetre or two: the middle favours neither
                    const PairSpec& p = *spec.pair(b.pair);
                    double gx = 0, gy = 0;
                    spec.frame.to_world(p.x, p.y, gx, gy);
                    r.ground = ground(gx, gy);
                    r.oz = r.ground + 0.5 * (cal.clip(p.clip_a)->zoff + cal.clip(p.clip_b)->zoff) - spec.sink;
                    r.partner_clip = b.pair_role == 'a' ? p.clip_b : p.clip_a;
                } else if (on_seat) {   // on the seat, where the game put a person: not lowered
                    r.seated = true;
                    r.ground = piece->z;
                    r.oz = piece->z + seated->z;
                }
                double rx = 0, ry = 0;
                rotate(r.heading, cc.root_x[static_cast<size_t>(j_last)], cc.root_y[static_cast<size_t>(j_last)], rx, ry);
                body.x = r.ox + rx; body.y = r.oy + ry;
                body.heading = norm_heading(r.heading + cc.yaw[static_cast<size_t>(j_last)]);
            }
            body.valid = true;
            body.clip = &cc;
            body.j = j_last;
            body.once = b.kind == BeatKind::Once || b.kind == BeatKind::Pair;
            ap.beats.push_back(r);
            tick += r.ticks;
            ++bi;
        }
        out.actors.push_back(ap);
    }
    // every hand-over is one 'a' half and one 'b' half starting on the same tick
    for (size_t i = 0; i < out.actors.size(); ++i)
        for (const auto& bi : out.actors[i].beats) {
            if (bi.kind != BeatKind::Pair) continue;
            int as = 0, bs = 0;
            for (const auto& ak : out.actors)
                for (const auto& bk : ak.beats)
                    if (bk.kind == BeatKind::Pair && bk.pair_index == bi.pair_index && bk.start == bi.start) ++(bk.pair_role == 'a' ? as : bs);
            if (as != 1 || bs != 1)
                return "pair " + spec.pairs[static_cast<size_t>(bi.pair_index)].id + " at tick " + std::to_string(bi.start) + ": " +
                       std::to_string(as) + " 'a' and " + std::to_string(bs) + " 'b' halves (" + out.actors[i].id +
                       " among them) -- a hand-over is one of each; give the partners' earlier beats equal lengths";
        }
    return "";
}

TickOp tick_op(const Plan& plan, const Calibration& cal, int actor, int tick) {
    TickOp op;
    if (actor < 0 || actor >= static_cast<int>(plan.actors.size()) || tick < 0 || tick >= plan.ticks) return op;
    const auto& beats = plan.actors[static_cast<size_t>(actor)].beats;
    for (size_t i = 0; i < beats.size(); ++i) {
        const ResolvedBeat& r = beats[i];
        if (tick < r.start || tick >= r.start + r.ticks) continue;
        const ClipCal& cc = *cal.clip(r.clip);
        const int u = tick - r.start;
        op.beat = static_cast<int>(i);
        op.enter = u == 0;
        op.blend_in = r.blend_in;
        op.shared = r.kind == BeatKind::Pair;
        if (r.kind == BeatKind::Walk) {
            const int j = (r.phase0 + u) % cc.n;
            const Path& p = plan.paths[static_cast<size_t>(r.path)];
            double px = 0, py = 0, pz = 0, h = 0;
            p.at(r.speed * (u + 0.5) / kTickHz, px, py, pz, h);
            double rx = 0, ry = 0;
            rotate(h, cc.root_x[static_cast<size_t>(j)], cc.root_y[static_cast<size_t>(j)], rx, ry);
            op.ox = px - rx; op.oy = py - ry; op.oz = pz + cc.zoff - plan.sink + plan.lift(r.clip) - plan.contact_correction(cc, j, true);
            op.heading = h;
            op.phase = static_cast<float>((j + 0.5) / cc.n);
            op.want_x = px; op.want_y = py; op.ground = pz; op.walking = true;
        } else {
            op.ox = r.ox; op.oy = r.oy; op.oz = r.oz; op.heading = r.heading;
            if (r.kind == BeatKind::Idle) op.phase = static_cast<float>(((r.phase0 + u) % cc.n + 0.5) / cc.n);
            else op.phase = static_cast<float>((std::min(u, r.cycle - 1) + 0.5) / r.cycle);   // played once
            op.ground = r.ground;
            // the feet on the sand phase by phase (not on a seat, which the game measured); a pair's one scene moves by
            // the middle of its two halves'
            if (!r.seated) {
                const auto at = [&op](const ClipCal& c) { return std::min(c.n - 1, static_cast<int>(op.phase * c.n)); };
                double corr = plan.contact_correction(cc, at(cc), false);
                if (r.kind == BeatKind::Pair && r.partner_clip.size()) {
                    const ClipCal* other = cal.clip(r.partner_clip);
                    if (other) corr = 0.5 * (corr + plan.contact_correction(*other, at(*other), false));
                }
                op.oz -= corr;
                op.contact = corr != 0.0;
            }
        }
        return op;
    }
    return op;
}

std::string render_plan(const Plan& plan) {
    std::ostringstream o;
    o << "dm-gta-stage-plan/1\n";
    o << "frame " << fmt(plan.frame.x) << " " << fmt(plan.frame.y) << " " << fmt(plan.frame.z) << " "
      << fmt(plan.frame.heading_deg) << "\n";
    o << "ticks " << plan.ticks << "\n";
    if (plan.sink != 0.0) o << "sink " << fmt(plan.sink) << "\n";
    if (plan.leg_ik >= 0) o << "legik " << plan.leg_ik << " " << plan.scene_ik_flags << "\n";
    for (const auto& l : plan.lifts) o << "lift " << l.clip << " " << fmt(l.metres) << "\n";
    for (const auto& s : plan.sets)
        o << "set " << s.id << " at " << fmt(s.x) << " " << fmt(s.y) << " " << fmt(s.z) << " heading " << fmt(s.heading) << "\n";
    for (size_t i = 0; i < plan.paths.size(); ++i) {
        const Path& p = plan.paths[i];
        // every ground height along the path goes into the digest: each take probes
        // its own, and a take whose walker would walk at another height is another plan
        std::uint64_t digest = 1469598103934665603ull;   // FNV-1a
        for (const double z : p.z)
            for (const char ch : fmt(z)) digest = (digest ^ static_cast<unsigned char>(ch)) * 1099511628211ull;
        char hex[17];
        std::snprintf(hex, sizeof(hex), "%016llx", static_cast<unsigned long long>(digest));
        o << "path " << i << " loop " << (p.loop ? 1 : 0) << " samples " << p.s.size() << " length " << fmt(p.total())
          << " from " << fmt(p.x.front()) << " " << fmt(p.y.front()) << " " << fmt(p.z.front())
          << " to " << fmt(p.x.back()) << " " << fmt(p.y.back()) << " " << fmt(p.z.back()) << " ground_digest " << hex << "\n";
    }
    for (const auto& a : plan.actors) {
        o << "actor " << a.id << " beats " << a.beats.size() << "\n";
        for (const auto& r : a.beats) {
            const char* k = r.kind == BeatKind::Idle ? "idle" : r.kind == BeatKind::Walk ? "walk" : r.kind == BeatKind::Once ? "once" : "pair";
            o << "  " << k << " " << r.clip << " start " << r.start << " ticks " << r.ticks << " phase0 " << r.phase0
              << " cycle " << r.cycle << " path " << r.path << " speed " << fmt(r.speed) << " origin " << fmt(r.ox) << " " << fmt(r.oy) << " " << fmt(r.oz)
              << " heading " << fmt(r.heading) << " ground " << fmt(r.ground) << " entry_pop " << fmt(r.entry_pop)
              << " blend " << fmt(r.blend_in) << "\n";
        }
    }
    return o.str();
}

}  // namespace diogenes::gta::stage
