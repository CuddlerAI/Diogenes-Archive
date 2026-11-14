#include "stage_runtime.hpp"

#include <main.h>
#include <natives.h>
#include <windows.h>

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <set>

namespace diogenes::gta::stage {

// SKEL_ROOT, SKEL_Spine3, SKEL_Head, SKEL_L_Hand, SKEL_R_Hand, SKEL_L_Foot, SKEL_R_Foot.
// A wrong id is not an error to GET_PED_BONE_COORDS: it answers with the root,
// and every bone then reports one point -- two runs agree perfectly for the
// wrong reason. Calibration refuses a pose whose bones collapse.
const std::array<int, kSampleBones> kSampleBoneIds{{0x0000, 0x60F2, 0x796E, 0x49D9, 0xDEAD, 0x3779, 0xCC4D}};
const std::array<const char*, kSampleBones> kSampleBoneNames{{"ROOT", "Spine3", "Head", "L_Hand", "R_Hand", "L_Foot", "R_Foot"}};

namespace {

constexpr double kPi = 3.14159265358979323846;
constexpr float kSnapIn = 1000.0f;           // TASK_SYNCHRONIZED_SCENE blend-in: effectively instant
constexpr float kRejoinIn = 2.0f;            // back into the plan: half a second from wherever it stands
constexpr int kRetireAfterBlend = 15;        // ticks a scene is kept to be blended out of
constexpr double kFootRefM = 0.08, kFootRefMaxM = 0.20;   // a standing ped's lowest foot bone above the sand

Hash hash_of(const std::string& s) { return GAMEPLAY::GET_HASH_KEY(const_cast<char*>(s.c_str())); }
// a spec's model: a name, or 0x<hash> as the take's map-scan files list it
Hash model_hash(const std::string& s) {
    return s.rfind("0x", 0) == 0 ? static_cast<Hash>(std::strtoul(s.c_str() + 2, nullptr, 16)) : hash_of(s);
}
// every object of the game's pool of the model within radius metres (on the ground plane) of world (x, y)
std::vector<int> objects_of(unsigned model, double x, double y, double radius) {
    static int handles[4096];
    const int n = worldGetAllObjects(handles, 4096);
    std::vector<int> out;
    for (int i = 0; i < n; ++i) {
        const Object o = handles[i];
        if (!ENTITY::DOES_ENTITY_EXIST(o) || static_cast<unsigned>(ENTITY::GET_ENTITY_MODEL(o)) != model) continue;
        const Vector3 v = ENTITY::GET_ENTITY_COORDS(o, TRUE);
        if (std::hypot(v.x - x, v.y - y) <= radius) out.push_back(o);
    }
    return out;
}
// every boat and jet ski (vehicle class 14) within radius metres of world (x, y) nobody plays
std::vector<int> boats_of(double x, double y, double radius) {
    static int handles[1024];
    const int n = worldGetAllVehicles(handles, 1024);
    std::vector<int> out;
    for (int i = 0; i < n; ++i) {
        const Vehicle v = handles[i];
        if (!ENTITY::DOES_ENTITY_EXIST(v) || VEHICLE::GET_VEHICLE_CLASS(v) != 14) continue;
        const Ped driver = VEHICLE::GET_PED_IN_VEHICLE_SEAT(v, -1);
        if (driver != 0 && PED::IS_PED_A_PLAYER(driver)) continue;
        const Vector3 at = ENTITY::GET_ENTITY_COORDS(v, TRUE);
        if (std::hypot(at.x - x, at.y - y) <= radius) out.push_back(v);
    }
    return out;
}
char* cstr(const std::string& s) { return const_cast<char*>(s.c_str()); }

std::string fmt(double v, int p = 3) {
    char b[48];
    std::snprintf(b, sizeof(b), "%.*f", p, v);
    return b;
}

double lowest_foot(Ped p) {
    const Vector3 l = PED::GET_PED_BONE_COORDS(p, 0x3779, 0.f, 0.f, 0.f);
    const Vector3 r = PED::GET_PED_BONE_COORDS(p, 0xCC4D, 0.f, 0.f, 0.f);
    return std::min(l.z, r.z);
}
double lowest_toe(Ped p) {   // L_Toe0, R_Toe0: the ball of the foot
    const Vector3 l = PED::GET_PED_BONE_COORDS(p, 0x083C, 0.f, 0.f, 0.f);
    const Vector3 r = PED::GET_PED_BONE_COORDS(p, 0x512D, 0.f, 0.f, 0.f);
    return std::min(l.z, r.z);
}

double wrap180(double d) {
    d = std::fmod(d + 540.0, 360.0) - 180.0;
    return d;
}

}  // namespace

Runtime::~Runtime() {
    // A runtime going away mid-take must not leave actors frozen in scenes.
    bool placed = false;
    for (const auto& piece : pieces_) placed = placed || piece.entity != 0;
    if (!cast_.empty() || !props_.empty() || placed) despawn();
    release_assets();
}

void Runtime::release_assets() {
    for (const auto& d : dicts_) STREAMING::REMOVE_ANIM_DICT(cstr(d));
    for (const unsigned m : models_) STREAMING::SET_MODEL_AS_NO_LONGER_NEEDED(m);
    dicts_.clear();
    models_.clear();
}

void Runtime::step(int frames) {
    for (int i = 0; i < frames; ++i) hooks_.frame();
}

std::string Runtime::clip_dict(const std::string& clip_id) const {
    const ClipSpec* c = spec_.clip(clip_id);
    return c ? c->dict : std::string();
}
std::string Runtime::clip_name(const std::string& clip_id) const {
    const ClipSpec* c = spec_.clip(clip_id);
    return c ? c->clip : std::string();
}

std::string Runtime::load_assets(const StageSpec& spec) {
    spec_ = spec;
    std::set<std::string> dicts, models;
    for (const auto& c : spec.clips) dicts.insert(c.dict);
    for (const auto& a : spec.actors) {
        models.insert(a.model);
        for (const auto& p : a.props) models.insert(p.model);
    }
    for (const auto& d : dicts) {
        STREAMING::REQUEST_ANIM_DICT(cstr(d));
        dicts_.push_back(d);
        int f = 0;
        for (; f < 300 && !STREAMING::HAS_ANIM_DICT_LOADED(cstr(d)); ++f) step(1);
        if (!STREAMING::HAS_ANIM_DICT_LOADED(cstr(d))) return "animation dictionary " + d + " did not load";
    }
    for (const auto& c : spec.clips) {
        // A dictionary that loads does not prove the clip name exists in it.
        const float dur = ENTITY::_GET_ANIM_DURATION(cstr(c.dict), cstr(c.clip));
        if (!(dur > 0.0f)) return "clip " + c.dict + " / " + c.clip + " has no duration -- not in its dictionary";
    }
    for (const auto& m : models) {
        const Hash h = hash_of(m);
        if (!STREAMING::IS_MODEL_IN_CDIMAGE(h)) return "model " + m + " is not in the game";
        STREAMING::REQUEST_MODEL(h);
        models_.push_back(h);
        int f = 0;
        for (; f < 300 && !STREAMING::HAS_MODEL_LOADED(h); ++f) step(1);
        if (!STREAMING::HAS_MODEL_LOADED(h)) return "model " + m + " did not load";
    }
    // set pieces: the first of each one's models the game has
    pieces_.clear();
    for (const auto& s : spec.sets) {
        Piece piece;
        piece.id = s.id;
        for (const auto& m : s.models)
            if (STREAMING::IS_MODEL_IN_CDIMAGE(hash_of(m))) { piece.name = m; piece.model = hash_of(m); break; }
        if (piece.name.empty()) return "set " + s.id + ": none of its models is in the game";
        piece.vehicle = STREAMING::IS_MODEL_A_VEHICLE(piece.model) != FALSE;
        STREAMING::REQUEST_MODEL(piece.model);
        models_.push_back(piece.model);
        int f = 0;
        for (; f < 300 && !STREAMING::HAS_MODEL_LOADED(piece.model); ++f) step(1);
        if (!STREAMING::HAS_MODEL_LOADED(piece.model)) return "set " + s.id + ": " + piece.name + " did not load";
        if (piece.name != s.models.front()) hooks_.log("  set " + s.id + ": " + s.models.front() + " is not in the game -- " + piece.name);
        pieces_.push_back(piece);
    }
    return "";
}

// A set piece at (x, y) on ground z, facing heading, frozen where it is put: an object
// rests on the ground the game's way (whatever height its model's origin has), a
// vehicle is parked -- set on its wheels, engine off, doors locked, no radio. Returns 0
// if the game made nothing.
int Runtime::place(const Piece& piece, double x, double y, double z, double heading) {
    int e = 0;
    if (piece.vehicle) {
        const Vehicle v = VEHICLE::CREATE_VEHICLE(piece.model, static_cast<float>(x), static_cast<float>(y), static_cast<float>(z + 0.3),
                                                  static_cast<float>(heading), FALSE, FALSE);
        if (!ENTITY::DOES_ENTITY_EXIST(v)) return 0;
        VEHICLE::SET_VEHICLE_ON_GROUND_PROPERLY(v);
        VEHICLE::SET_VEHICLE_ENGINE_ON(v, FALSE, TRUE, TRUE);
        VEHICLE::SET_VEHICLE_DOORS_LOCKED(v, 2);
        AUDIO::SET_VEHICLE_RADIO_ENABLED(v, FALSE);
        e = v;
    } else {
        const Object o = OBJECT::CREATE_OBJECT_NO_OFFSET(piece.model, static_cast<float>(x), static_cast<float>(y), static_cast<float>(z),
                                                         FALSE, TRUE, FALSE);
        if (!ENTITY::DOES_ENTITY_EXIST(o)) return 0;
        ENTITY::SET_ENTITY_HEADING(o, static_cast<float>(heading));
        OBJECT::PLACE_OBJECT_ON_GROUND_PROPERLY(o);
        e = o;
    }
    ENTITY::SET_ENTITY_AS_MISSION_ENTITY(e, TRUE, TRUE);   // ours: the population code never clears it
    ENTITY::SET_ENTITY_INVINCIBLE(e, TRUE);
    ENTITY::FREEZE_ENTITY_POSITION(e, TRUE);
    return e;
}

void Runtime::remove(Piece& piece) {
    if (piece.entity == 0) return;
    ENTITY::SET_ENTITY_AS_MISSION_ENTITY(piece.entity, TRUE, TRUE);
    if (piece.vehicle) { Vehicle v = piece.entity; VEHICLE::DELETE_VEHICLE(&v); }
    else { Object o = piece.entity; OBJECT::DELETE_OBJECT(&o); }
    piece.entity = 0;
}

double Runtime::ground(double x, double y) {
    float gz = 0.f;
    if (GAMEPLAY::GET_GROUND_Z_FOR_3D_COORD(static_cast<float>(x), static_cast<float>(y), static_cast<float>(spec_.frame.z + 5.0), &gz, 0) &&
        std::fabs(gz - spec_.frame.z) < 5.0) {
        last_ground_ = gz;
        have_ground_ = true;
        return gz;
    }
    // A miss falls back to a height already known to be on the ground here --
    // never to a ped's root height, which would hang a scene a metre up.
    ++ground_misses_;
    return have_ground_ ? last_ground_ : spec_.frame.z;
}

std::string Runtime::calibrate(const StageSpec& spec, Calibration& out) {
    spec_ = spec;
    out = Calibration{};
    double wx = 0, wy = 0;
    spec.frame.to_world(0.0, 0.0, wx, wy);   // mid-stage: sand the cast is not standing on yet
    const double gz = ground(wx, wy);
    for (const auto& c : spec.clips) {
        // measured on the model of an actor that plays it, dressed as that actor is: shoes
        // change where the feet go (the tourist's heels under a man's photo clip put her toes
        // into the sand, the customer 2026-09-29)
        std::string model;
        const ActorSpec* dressed = nullptr;
        for (const auto& a : spec.actors) {
            for (const auto& b : a.beats) {
                std::string id = b.clip;
                if (b.kind == BeatKind::Pair) {
                    const PairSpec* p = spec.pair(b.pair);
                    id = b.pair_role == 'a' ? p->clip_a : p->clip_b;
                }
                if (id == c.id) { model = a.model; dressed = &a; break; }
            }
            if (!model.empty()) break;
        }
        if (model.empty()) return "clip " + c.id + " is declared but no actor plays it";

        const Ped cp = PED::CREATE_PED(4, hash_of(model), static_cast<float>(wx), static_cast<float>(wy), static_cast<float>(gz + 1.0), 0.0f, FALSE, TRUE);
        if (!ENTITY::DOES_ENTITY_EXIST(cp)) return "could not spawn " + model + " to measure " + c.id;
        PED::SET_PED_DEFAULT_COMPONENT_VARIATION(cp);
        for (const auto& l : dressed->looks) PED::SET_PED_COMPONENT_VARIATION(cp, l.component, l.drawable, l.texture, 0);
        PED::SET_BLOCKING_OF_NON_TEMPORARY_EVENTS(cp, TRUE);
        AI::TASK_STAND_STILL(cp, 20000);   // left to itself a fresh ped may wander off the spot it is measured on

        // Standing naturally on the sand is where feet belong -- once it stands:
        // v4 read this 45 frames after the spawn, still falling, and floated the
        // whole cast 73 cm. Settled: 2 s on, not falling nor in the air, and its
        // height steady to 2 mm for 90 frames running.
        const double t0 = hooks_.now_s();
        float last = ENTITY::GET_ENTITY_COORDS(cp, TRUE).z;
        int still = 0;
        bool settled = false;
        while (hooks_.now_s() - t0 < 8.0) {
            step(1);
            const float z = ENTITY::GET_ENTITY_COORDS(cp, TRUE).z;
            const bool grounded = !PED::IS_PED_FALLING(cp) && !ENTITY::IS_ENTITY_IN_AIR(cp);
            still = (grounded && std::fabs(z - last) < 0.002f) ? still + 1 : 0;
            last = z;
            if (hooks_.now_s() - t0 >= 2.0 && still >= 90) { settled = true; break; }
        }
        const Vector3 at = ENTITY::GET_ENTITY_COORDS(cp, TRUE);
        double foot_ref = lowest_foot(cp) - ground(at.x, at.y);
        // and its toes, standing: a clip that points a foot (heels, a step) can bury the toe with the ankle still up
        double toe_ref = lowest_toe(cp) - ground(at.x, at.y);
        const bool toe_ok = settled && toe_ref >= 0.0 && toe_ref <= kFootRefMaxM;
        std::string note = ", stood in " + fmt(hooks_.now_s() - t0, 1) + " s";
        if (!settled || foot_ref < 0.0 || foot_ref > kFootRefMaxM) {
            hooks_.log("  CALIB " + c.id + ": the standing reference read " + fmt(foot_ref * 100, 1) + " cm" +
                       (settled ? "" : " (it never settled)") + " -- using " + fmt(kFootRefM * 100, 0) + " cm");
            foot_ref = kFootRefM;
            note += ", FALLBACK";
        }

        const int sc = PED::CREATE_SYNCHRONIZED_SCENE(static_cast<float>(wx), static_cast<float>(wy), static_cast<float>(gz), 0.0f, 0.0f, 0.0f, 2);
        if (sc < 0) {
            ENTITY::SET_ENTITY_AS_MISSION_ENTITY(cp, TRUE, TRUE); Ped h = cp; PED::DELETE_PED(&h);
            return "no synchronized scene to measure " + c.id;
        }
        PED::SET_SYNCHRONIZED_SCENE_LOOPED(sc, TRUE);
        PED::SET_SYNCHRONIZED_SCENE_RATE(sc, 0.0f);
        AI::CLEAR_PED_TASKS_IMMEDIATELY(cp);
        AI::TASK_SYNCHRONIZED_SCENE(cp, sc, cstr(c.dict), cstr(c.clip), kSnapIn, -4.0f, 0, 0, 1000.0f, 0);
        step(3);

        ClipCal cal;
        cal.id = c.id;
        cal.duration_s = ENTITY::_GET_ANIM_DURATION(cstr(c.dict), cstr(c.clip));
        cal.n = std::max(2, static_cast<int>(std::lround(cal.duration_s * kTickHz)));
        double low = 1e9;
        bool collapsed = true;
        std::vector<double> ankles, toes;   // per phase, absolute heights, the origin at gz
        for (int j = 0; j < cal.n; ++j) {
            PED::SET_SYNCHRONIZED_SCENE_PHASE(sc, static_cast<float>((j + 0.5) / cal.n));
            step(2);   // two frames: the settled pose, not last frame's
            const Vector3 v = ENTITY::GET_ENTITY_COORDS(cp, TRUE);
            const double h = ENTITY::GET_ENTITY_HEADING(cp);
            cal.root_x.push_back(v.x - wx);
            cal.root_y.push_back(v.y - wy);
            cal.yaw.push_back(wrap180(h));
            const Vector3 root = PED::GET_PED_BONE_COORDS(cp, kSampleBoneIds[0], 0.f, 0.f, 0.f);
            const double hr = -h * kPi / 180.0;
            for (int b = 1; b < kSampleBones; ++b) {
                const Vector3 p = PED::GET_PED_BONE_COORDS(cp, kSampleBoneIds[static_cast<size_t>(b)], 0.f, 0.f, 0.f);
                const double dx = p.x - root.x, dy = p.y - root.y, dz = p.z - root.z;
                if (std::sqrt(dx * dx + dy * dy + dz * dz) > 0.05) collapsed = false;
                cal.pose.push_back(dx * std::cos(hr) - dy * std::sin(hr));   // into the entity's own frame
                cal.pose.push_back(dx * std::sin(hr) + dy * std::cos(hr));
                cal.pose.push_back(dz);
            }
            low = std::min(low, lowest_foot(cp));
            ankles.push_back(lowest_foot(cp));
            toes.push_back(lowest_toe(cp));
        }
        cal.zoff = (gz + foot_ref) - low;
        // per phase: how far the lowest contact is over where it stands, the origin zoff over the ground
        double cmin = 1e9, cmax = -1e9;
        for (int j = 0; j < cal.n; ++j) {
            double c = ankles[static_cast<size_t>(j)] - low;
            if (toe_ok) c = std::min(c, toes[static_cast<size_t>(j)] - gz + cal.zoff - toe_ref);
            cal.contact.push_back(c);
            cmin = std::min(cmin, c); cmax = std::max(cmax, c);
        }
        note += ", contact " + fmt(cmin * 100, 1) + ".." + fmt(cmax * 100, 1) + " cm" + (toe_ok ? "" : " (ankles only)");
        if (c.kind == ClipKind::Walk) {
            const double L = std::hypot(cal.root_x.back() - cal.root_x.front(), cal.root_y.back() - cal.root_y.front()) * cal.n / (cal.n - 1);
            cal.speed = L / cal.duration_s;
        }
        AI::CLEAR_PED_TASKS_IMMEDIATELY(cp);
        retire_scene(sc);
        ENTITY::SET_ENTITY_AS_MISSION_ENTITY(cp, TRUE, TRUE);
        Ped h = cp; PED::DELETE_PED(&h);
        if (collapsed) return "clip " + c.id + ": every bone reads the root -- the bone ids do not resolve on " + model;
        hooks_.log("  calibrated " + c.id + " on " + model + (dressed->looks.empty() ? "" : " as " + dressed->id + " dresses") + ": " +
                   fmt(cal.duration_s, 3) + " s = " + std::to_string(cal.n) +
                   " ticks, origin " + fmt(cal.zoff, 3) + " m above the ground (standing foot " + fmt(foot_ref * 100, 1) + " cm" + note + ")" +
                   (c.kind == ClipKind::Walk ? ", " + fmt(cal.speed, 3) + " m/s" : std::string()));
        if (c.kind == ClipKind::Walk && cal.speed < 0.3) return "walk clip " + c.id + " barely moves (" + fmt(cal.speed, 2) + " m/s)";
        out.clips.push_back(cal);
    }
    // Seats: where the game itself seats a person on the piece. The piece goes on the
    // sand mid-stage facing world heading 0, a person of the actor's model is warped into
    // the nearest scenario there -- the piece's own "sit here" -- and the scene that puts
    // the actor's seated clip on that spot, in the piece's frame, is what the plan uses.
    for (const auto& seat : spec.seats) {
        SeatCal sc;
        sc.actor = seat.actor;
        Piece* piece = nullptr;
        for (auto& p : pieces_) if (p.id == seat.set) piece = &p;
        const ActorSpec* actor = nullptr;
        for (const auto& a : spec.actors) if (a.id == seat.actor) actor = &a;
        const ClipCal* cc = out.clip(seat.clip);
        const ClipSpec* cs = spec.clip(seat.clip);
        if (!piece || !actor || !cc || !cs) return "seat " + seat.actor + ": its piece, actor or clip is missing";
        Piece probe = *piece;
        probe.entity = place(probe, wx, wy, gz, 0.0);
        if (probe.entity == 0) return "seat " + seat.actor + ": " + probe.name + " did not spawn to be measured";
        step(10);
        const Ped p = PED::CREATE_PED(4, hash_of(actor->model), static_cast<float>(wx + 2.0), static_cast<float>(wy), static_cast<float>(gz + 1.0),
                                      0.0f, FALSE, TRUE);
        if (!ENTITY::DOES_ENTITY_EXIST(p)) { remove(probe); return "seat " + seat.actor + ": could not spawn " + actor->model; }
        PED::SET_PED_DEFAULT_COMPONENT_VARIATION(p);
        PED::SET_BLOCKING_OF_NON_TEMPORARY_EVENTS(p, TRUE);
        step(30);
        AI::TASK_USE_NEAREST_SCENARIO_TO_COORD_WARP(p, static_cast<float>(wx), static_cast<float>(wy), static_cast<float>(gz + 0.5), 1.0f, 60000);
        // seated: in the scenario, on the piece, and still for two seconds
        const double t0 = hooks_.now_s();
        Vector3 last = ENTITY::GET_ENTITY_COORDS(p, TRUE);
        int still = 0;
        bool seated = false;
        while (hooks_.now_s() - t0 < 10.0) {
            step(1);
            const Vector3 v = ENTITY::GET_ENTITY_COORDS(p, TRUE);
            const double moved = std::sqrt((v.x - last.x) * (v.x - last.x) + (v.y - last.y) * (v.y - last.y) + (v.z - last.z) * (v.z - last.z));
            const bool on_it = PED::IS_PED_USING_ANY_SCENARIO(p) != FALSE && std::hypot(v.x - wx, v.y - wy) < 1.2;
            still = on_it && moved < 0.002 ? still + 1 : 0;
            last = v;
            if (still >= 60) { seated = true; break; }
        }
        if (seated) {
            const Vector3 at = ENTITY::GET_ENTITY_COORDS(p, TRUE);
            const double h = ENTITY::GET_ENTITY_HEADING(p);
            // the scene that puts our clip's person there, facing that way (phase 0 of the loop)
            const double oh = norm_heading(h - cc->yaw[0]);
            const double c = std::cos(oh * kPi / 180.0), s = std::sin(oh * kPi / 180.0);
            const double ox = at.x - (cc->root_x[0] * c - cc->root_y[0] * s), oy = at.y - (cc->root_x[0] * s + cc->root_y[0] * c);
            // its height: once at the seated person's own height, then moved by what that
            // misses -- with the seat's collision off, as on the stage (spawn)
            AI::CLEAR_PED_TASKS_IMMEDIATELY(p);
            ENTITY::SET_ENTITY_COLLISION(probe.entity, FALSE, FALSE);
            const int ss = PED::CREATE_SYNCHRONIZED_SCENE(static_cast<float>(ox), static_cast<float>(oy), at.z, 0.0f, 0.0f, static_cast<float>(oh), 2);
            if (ss >= 0) {
                PED::SET_SYNCHRONIZED_SCENE_LOOPED(ss, TRUE);
                PED::SET_SYNCHRONIZED_SCENE_RATE(ss, 0.0f);
                AI::TASK_SYNCHRONIZED_SCENE(p, ss, cstr(cs->dict), cstr(cs->clip), kSnapIn, -4.0f, 0, 0, 1000.0f, 0);
                PED::SET_SYNCHRONIZED_SCENE_PHASE(ss, static_cast<float>(0.5 / cc->n));
                step(3);
                const Vector3 ours = ENTITY::GET_ENTITY_COORDS(p, TRUE);
                const double oz = at.z + (at.z - ours.z);
                const double off = std::hypot(ours.x - at.x, ours.y - at.y);
                AI::CLEAR_PED_TASKS_IMMEDIATELY(p);
                retire_scene(ss);
                sc.found = true;
                sc.x = ox - wx; sc.y = oy - wy; sc.z = oz - gz; sc.heading = oh;   // the piece stood at (wx, wy, gz) facing 0
                hooks_.log("  seat " + seat.actor + " on " + probe.name + ": the game sits a person " + fmt(at.z - gz, 3) + " m up, " +
                           fmt(std::hypot(at.x - wx, at.y - wy), 3) + " m from the piece; scene " + fmt(sc.x, 3) + " " + fmt(sc.y, 3) +
                           " " + fmt(sc.z, 3) + " heading " + fmt(sc.heading, 1) + " in its frame; " + seat.clip + " lands " +
                           fmt(off * 100, 1) + " cm off it");
            }
        }
        ENTITY::SET_ENTITY_AS_MISSION_ENTITY(p, TRUE, TRUE);
        Ped dead = p;
        PED::DELETE_PED(&dead);
        remove(probe);
        // An actor sitting on air is no take: stop here, before the recorder, and say which chair.
        if (!sc.found)
            return "seat " + seat.actor + ": the game seated nobody on " + probe.name +
                   " in 10 s -- name first in its set line a chair the game seats people on";
        out.seats.push_back(sc);
    }
    return "";
}

std::string Runtime::spawn(const StageSpec& spec, const Plan& plan, const Calibration& cal) {
    spec_ = spec;
    despawn();
    if (plan.sets.size() != pieces_.size()) return "the plan places " + std::to_string(plan.sets.size()) + " set pieces, " +
                                                   std::to_string(pieces_.size()) + " were loaded";
    for (size_t i = 0; i < pieces_.size(); ++i) {   // the set first: a seated actor sits on its piece from tick 0
        const SetPlacement& at = plan.sets[i];
        Piece& piece = pieces_[i];
        if (at.id != piece.id) return "set piece " + at.id + " is not the one loaded (" + piece.id + ")";
        piece.entity = place(piece, at.x, at.y, at.z, at.heading);
        if (piece.entity == 0) return "set " + piece.id + " (" + piece.name + ") did not spawn";
        // a seat is scenery for its sitter, who is put there by its clip: nothing to push him about
        for (const auto& seat : spec.seats) if (seat.set == piece.id) ENTITY::SET_ENTITY_COLLISION(piece.entity, FALSE, FALSE);
        const Vector3 v = ENTITY::GET_ENTITY_COORDS(piece.entity, TRUE);
        hooks_.log("  placed " + piece.id + " (" + piece.name + ") at " + fmt(v.x, 2) + " " + fmt(v.y, 2) + " " + fmt(v.z, 2) +
                   " heading " + fmt(ENTITY::GET_ENTITY_HEADING(piece.entity), 1));
    }
    // The map's own things the spec takes out (clear lines): deleted, and the model hidden there so the map does not
    // bring them back while the take runs. Before the recorder starts: not in the recording from its first frame.
    clears_.clear();
    cleared_ = 0;
    for (const auto& cs : spec.clears) {
        Clear c;
        c.boats = cs.model == "boats";
        c.model = c.boats ? 0 : model_hash(cs.model);
        spec.frame.to_world(cs.x, cs.y, c.x, c.y);
        c.z = spec.frame.z;
        c.radius = cs.radius;
        int removed = 0;
        if (c.boats) {
            const float r = static_cast<float>(c.radius), x = static_cast<float>(c.x), y = static_cast<float>(c.y), z = static_cast<float>(c.z);
            VEHICLE::SET_ALL_VEHICLE_GENERATORS_ACTIVE_IN_AREA(x - r, y - r, z - 50.0f, x + r, y + r, z + 50.0f, FALSE, FALSE);
            for (int v : boats_of(c.x, c.y, c.radius)) {
                const unsigned m = static_cast<unsigned>(ENTITY::GET_ENTITY_MODEL(v));
                if (std::find(c.suppressed.begin(), c.suppressed.end(), m) == c.suppressed.end()) c.suppressed.push_back(m);
                const int seats = VEHICLE::GET_VEHICLE_MAX_NUMBER_OF_PASSENGERS(v);
                for (int seat = -1; seat < seats; ++seat) {
                    Ped p = VEHICLE::GET_PED_IN_VEHICLE_SEAT(v, seat);
                    if (p == 0) continue;
                    ENTITY::SET_ENTITY_AS_MISSION_ENTITY(p, TRUE, TRUE);
                    PED::DELETE_PED(&p);
                }
                ENTITY::SET_ENTITY_AS_MISSION_ENTITY(v, TRUE, TRUE);
                Vehicle d = v;
                VEHICLE::DELETE_VEHICLE(&d);
                ++removed;
            }
            for (unsigned m : c.suppressed) VEHICLE::SET_VEHICLE_MODEL_IS_SUPPRESSED(m, TRUE);
        } else {
            for (int o : objects_of(c.model, c.x, c.y, c.radius)) {
                ENTITY::SET_ENTITY_AS_MISSION_ENTITY(o, TRUE, TRUE);
                Object d = o;
                OBJECT::DELETE_OBJECT(&d);
                ++removed;
            }
            ENTITY::CREATE_MODEL_HIDE_EXCLUDING_SCRIPT_OBJECTS(static_cast<float>(c.x), static_cast<float>(c.y), static_cast<float>(c.z),
                                                              static_cast<float>(c.radius), c.model, TRUE);
        }
        cleared_ += removed;
        clears_.push_back(c);
        hooks_.log("  clear " + cs.model + ": " + std::to_string(removed) + " removed within " + fmt(cs.radius, 0) + " m, " +
                   std::to_string(c.boats ? boats_of(c.x, c.y, c.radius).size() : objects_of(c.model, c.x, c.y, c.radius).size()) + " left");
    }
    // the map's own moving things, found where the spec says (not ours: never moved, never deleted)
    tracks_.clear();
    for (const auto& tr : spec.tracks) {
        Track t;
        t.id = tr.id;
        t.model = model_hash(tr.model);
        double wx = 0, wy = 0;
        spec.frame.to_world(tr.x, tr.y, wx, wy);
        t.object = OBJECT::GET_CLOSEST_OBJECT_OF_TYPE(static_cast<float>(wx), static_cast<float>(wy), static_cast<float>(spec.frame.z),
                                                      static_cast<float>(tr.radius), t.model, FALSE, FALSE, FALSE);
        if (t.object != 0 && ENTITY::DOES_ENTITY_EXIST(t.object)) {
            const Vector3 v = ENTITY::GET_ENTITY_COORDS(t.object, TRUE);
            hooks_.log("  track " + tr.id + " (" + tr.model + ") found at " + fmt(v.x, 2) + " " + fmt(v.y, 2) + " " + fmt(v.z, 2));
        } else {
            t.object = 0;
            hooks_.log("  track " + tr.id + " (" + tr.model + "): none within " + fmt(tr.radius, 1) + " m -- logged as absent");
        }
        tracks_.push_back(t);
    }
    cast_.assign(spec.actors.size(), Actor{});
    for (size_t i = 0; i < spec.actors.size(); ++i) {
        const ActorSpec& as = spec.actors[i];
        const TickOp op = tick_op(plan, cal, static_cast<int>(i), 0);
        if (op.beat < 0) return "actor " + as.id + " has nothing to do on tick 0";
        const ResolvedBeat& r = plan.actors[i].beats[static_cast<size_t>(op.beat)];
        const ClipCal& cc = *cal.clip(r.clip);
        const int j = std::min(cc.n - 1, static_cast<int>(op.phase * cc.n));
        const double h = op.heading * kPi / 180.0;
        const double bx = op.ox + cc.root_x[static_cast<size_t>(j)] * std::cos(h) - cc.root_y[static_cast<size_t>(j)] * std::sin(h);
        const double by = op.oy + cc.root_x[static_cast<size_t>(j)] * std::sin(h) + cc.root_y[static_cast<size_t>(j)] * std::cos(h);
        const Ped p = PED::CREATE_PED(4, hash_of(as.model), static_cast<float>(bx), static_cast<float>(by), static_cast<float>(op.ground + 1.0),
                                      static_cast<float>(op.heading), FALSE, TRUE);
        if (!ENTITY::DOES_ENTITY_EXIST(p)) return "actor " + as.id + " (" + as.model + ") did not spawn";
        cast_[i].ped = p;
        // CREATE_PED rolls a random outfit: pin it, then apply the spec's looks.
        PED::SET_PED_DEFAULT_COMPONENT_VARIATION(p);
        for (const auto& l : as.looks) PED::SET_PED_COMPONENT_VARIATION(p, l.component, l.drawable, l.texture, 0);
        PED::SET_BLOCKING_OF_NON_TEMPORARY_EVENTS(p, TRUE);
        PED::SET_PED_CAN_RAGDOLL(p, FALSE);
        ENTITY::SET_ENTITY_INVINCIBLE(p, TRUE);
        AUDIO::STOP_PED_SPEAKING(p, TRUE);
        PED::SET_PED_KEEP_TASK(p, TRUE);
        if (spec.leg_ik >= 0) PED::SET_PED_LEG_IK_MODE(p, spec.leg_ik);
        for (const auto& pr : as.props) {
            const Vector3 v = ENTITY::GET_ENTITY_COORDS(p, TRUE);
            const Object o = OBJECT::CREATE_OBJECT(hash_of(pr.model), v.x, v.y, v.z + 0.5f, FALSE, TRUE, FALSE);
            if (!ENTITY::DOES_ENTITY_EXIST(o)) return "prop " + pr.model + " for " + as.id + " did not spawn";
            // Pinned hard (no soft pinning), so a prop never drops off a released actor;
            // attached props are rigid -- v4 measured the camera 20.4-20.5 cm from the hand throughout.
            ENTITY::ATTACH_ENTITY_TO_ENTITY(o, p, PED::GET_PED_BONE_INDEX(p, pr.bone), 0.0f, 0.0f, 0.0f, 0.0f, 0.0f, 0.0f,
                                            TRUE, FALSE, FALSE, TRUE, 1, TRUE);
            props_.push_back({o, static_cast<int>(i), pr.model});
        }
    }
    step(2);
    return "";
}

int Runtime::new_scene(double x, double y, double z, double heading) {
    const int s = PED::CREATE_SYNCHRONIZED_SCENE(static_cast<float>(x), static_cast<float>(y), static_cast<float>(z), 0.0f, 0.0f,
                                                 static_cast<float>(heading), 2);
    ++scenes_created_;
    if (s < 0) { ++scene_failures_; return -1; }   // -1 must never reach another scene native
    if (std::find(scene_ids_.begin(), scene_ids_.end(), s) == scene_ids_.end()) scene_ids_.push_back(s);
    // Looped even though the phase is driven by hand: an unlooped scene retires
    // itself once its phase reaches the end of the clip.
    PED::SET_SYNCHRONIZED_SCENE_LOOPED(s, TRUE);
    PED::SET_SYNCHRONIZED_SCENE_RATE(s, 0.0f);
    return s;
}

// Scenes waiting to be retired. One queue for everybody: a pair's shared scene is
// left by two actors at once, and queueing it twice would, once its id had been
// recycled, retire somebody else's live scene the second time round.
void Runtime::queue_retire(int scene, int due) {
    if (scene < 0) return;
    for (auto& e : retire_) if (e.first == scene) { e.second = std::max(e.second, due); return; }
    retire_.push_back({scene, due});
}
bool Runtime::scene_in_use(int scene) const {
    for (const auto& a : cast_) if (a.ped != 0 && a.released_from < 0 && a.scene == scene) return true;
    return false;
}
// Both ways the engine might clean a scene up: run it off its end (a looped
// scene held at rate 0 never finishes by itself), and hand it to the disposer.
void Runtime::retire_scene(int scene) {
    if (scene < 0) return;
    // a retired pair scene leaves the list, so despawn() never retires it again
    shared_.erase(std::remove_if(shared_.begin(), shared_.end(), [scene](const Shared& s) { return s.scene == scene; }), shared_.end());
    PED::SET_SYNCHRONIZED_SCENE_LOOPED(scene, FALSE);
    PED::SET_SYNCHRONIZED_SCENE_PHASE(scene, 0.99f);
    PED::SET_SYNCHRONIZED_SCENE_RATE(scene, 1.0f);
    PED::_0xCD9CC7E200A52A6F(scene);   // disposer in the public native database; only ever given scenes nobody is in
}
void Runtime::retire_due(int k) {
    for (auto it = retire_.begin(); it != retire_.end();) {
        if (it->second > k) { ++it; continue; }
        if (!scene_in_use(it->first)) retire_scene(it->first);   // in use means its id was recycled: leave it
        it = retire_.erase(it);
    }
}

int Runtime::enter(const Plan& plan, const Calibration& cal, int k) {
    int entered = 0;
    for (size_t i = 0; i < cast_.size(); ++i) {
        Actor& a = cast_[i];
        if (a.ped == 0 || a.released_from >= 0) continue;
        const TickOp op = tick_op(plan, cal, static_cast<int>(i), k - a.delay);
        if (op.beat < 0 || op.beat == a.beat) continue;   // a beat is entered once, even across skipped ticks
        const ResolvedBeat& r = plan.actors[i].beats[static_cast<size_t>(op.beat)];
        int sc = -1;
        if (op.shared) {   // one scene per hand-over: this pair, this start
            for (const auto& s : shared_) if (s.pair == r.pair_index && s.start == r.start) sc = s.scene;
            if (sc < 0) {
                sc = new_scene(op.ox, op.oy, op.oz, op.heading);
                if (sc >= 0) shared_.push_back({r.pair_index, r.start, sc});
            }
        } else {
            sc = new_scene(op.ox, op.oy, op.oz, op.heading);
        }
        if (sc < 0) {   // the pool refused: this actor stops here rather than play the wrong thing
            hooks_.log("  SCENE " + spec_.actors[i].id + ": no scene for its beat at tick " + std::to_string(k) + " -- released");
            release(static_cast<int>(i), k);
            ++runtime_releases_;
            continue;
        }
        // A new scene starts at phase 0; the frames until pose() runs would show
        // the clip's first pose (a walker a stride away) -- start it at k's.
        PED::SET_SYNCHRONIZED_SCENE_PHASE(sc, op.phase);
        const int old = a.scene;
        a.scene = sc;
        a.beat = op.beat;
        // A snap clears the ped and lands at once: exact, the path proven
        // deterministic. A blend leaves the current scene running and lets the
        // engine cross-fade into the new one over real time: smooth, and the pose
        // differs slightly from run to run for a few ticks (the position does not,
        // the mover snaps). The old scene is kept until the blend is over: a blend
        // lasts at most a quarter second (7.5 ticks; the spec refuses slower ones).
        const bool blend = (r.blend_in > 0.0 && old >= 0) || a.rejoining;
        if (!blend) AI::CLEAR_PED_TASKS_IMMEDIATELY(a.ped);
        const float in = a.rejoining ? kRejoinIn : blend ? static_cast<float>(r.blend_in) : kSnapIn;
        AI::TASK_SYNCHRONIZED_SCENE(a.ped, sc, cstr(clip_dict(r.clip)), cstr(clip_name(r.clip)), in, -4.0f, 0, 0, 1000.0f,
                                    plan.scene_ik_flags);
        PED::SET_SYNCHRONIZED_SCENE_PHASE(sc, op.phase);
        if (old != sc) queue_retire(old, blend ? k + kRetireAfterBlend : k);
        entered_.push_back({k, static_cast<int>(i), sc, op.beat});
        a.rejoining = false;
        ++entered;
    }
    return entered;
}

std::string Runtime::verify(const Plan& plan, int upto) {
    std::string problems;
    std::vector<Entry> later;
    for (const Entry& e : entered_) {
        if (e.tick > upto) { later.push_back(e); continue; }
        Actor& a = cast_[static_cast<size_t>(e.actor)];
        // gone, released, or on to its next scene already: nothing left to check
        if (a.ped == 0 || a.released_from >= 0 || a.scene != e.scene) continue;
        const std::string& id = spec_.actors[static_cast<size_t>(e.actor)].id;
        PED::SET_SYNCHRONIZED_SCENE_RATE(a.scene, 0.0f);
        if (!PED::IS_SYNCHRONIZED_SCENE_RUNNING(a.scene)) {
            ++entries_not_running_;
            problems += id + ": scene entered at tick " + std::to_string(e.tick) + " not running; ";
            continue;
        }
        // "running" does not prove the ped went in: ask whether it plays the clip
        const ResolvedBeat& r = plan.actors[static_cast<size_t>(e.actor)].beats[static_cast<size_t>(e.beat)];
        ++entries_checked_;
        if (!ENTITY::IS_ENTITY_PLAYING_ANIM(a.ped, cstr(clip_dict(r.clip)), cstr(clip_name(r.clip)), 3)) {
            ++entries_not_playing_;
            problems += id + ": not playing " + r.clip + " entered at tick " + std::to_string(e.tick) + "; ";
        }
    }
    entered_ = std::move(later);
    return problems;
}

void Runtime::pose(const Plan& plan, const Calibration& cal, int k) {
    std::vector<int> posed;   // a shared scene takes its phase once per tick
    for (size_t i = 0; i < cast_.size(); ++i) {
        const Actor& a = cast_[i];
        if (a.ped == 0 || a.released_from >= 0 || a.scene < 0) continue;
        const TickOp op = tick_op(plan, cal, static_cast<int>(i), k - a.delay);
        if (op.beat < 0) continue;
        if (std::find(posed.begin(), posed.end(), a.scene) != posed.end()) continue;
        posed.push_back(a.scene);
        if (plan.leg_ik >= 0) PED::SET_PED_LEG_IK_MODE(a.ped, plan.leg_ik);   // again every tick: a task change may reset it
        if (op.walking || op.contact)   // last argument 0: undocumented here, and 0 is safe as either a rotation order or a flag
            PED::SET_SYNCHRONIZED_SCENE_ORIGIN(a.scene, static_cast<float>(op.ox), static_cast<float>(op.oy), static_cast<float>(op.oz),
                                               0.0f, 0.0f, static_cast<float>(op.heading), 0);
        PED::SET_SYNCHRONIZED_SCENE_PHASE(a.scene, op.phase);
    }
    // Hand props that only show in some clips (StageSpec propshow): out while the owner
    // plays one of them between its phases; away otherwise -- also once the owner is
    // released (taken over, or reacting to a rule), since it is then no clip of ours.
    for (Prop& p : props_) {
        const ActorSpec& as = spec_.actors[static_cast<size_t>(p.owner)];
        if (as.prop_shows.empty()) continue;
        const Actor& a = cast_[static_cast<size_t>(p.owner)];
        bool show = false;
        if (a.ped != 0 && a.released_from < 0 && a.scene >= 0) {
            const TickOp op = tick_op(plan, cal, p.owner, k - a.delay);
            if (op.beat >= 0) {
                const std::string& clip = plan.actors[static_cast<size_t>(p.owner)].beats[static_cast<size_t>(op.beat)].clip;
                for (const PropShow& s : as.prop_shows)
                    if (s.clip == clip && op.phase >= s.from && op.phase <= s.to) show = true;
            }
        }
        if (show != p.shown && ENTITY::DOES_ENTITY_EXIST(p.object)) {
            ENTITY::SET_ENTITY_VISIBLE(p.object, show, FALSE);
            p.shown = show;
        }
    }
}

void Runtime::release(int actor, int k, bool gently) {
    if (actor < 0 || actor >= actors()) return;
    Actor& a = cast_[static_cast<size_t>(actor)];
    if (a.released_from >= 0) return;
    a.released_from = k;
    if (a.ped != 0) {
        if (!gently) AI::CLEAR_PED_TASKS_IMMEDIATELY(a.ped);
        PED::SET_BLOCKING_OF_NON_TEMPORARY_EVENTS(a.ped, FALSE);
    }
    queue_retire(a.scene, gently ? k + kRetireAfterBlend : k);
    a.scene = -1;
}
bool Runtime::released(int actor) const { return released_from(actor) >= 0; }
int Runtime::released_from(int actor) const {
    return actor >= 0 && actor < actors() ? cast_[static_cast<size_t>(actor)].released_from : -1;
}
void Runtime::rejoin(int actor, int delay) {
    if (actor < 0 || actor >= actors()) return;
    Actor& a = cast_[static_cast<size_t>(actor)];
    if (a.ped == 0 || a.released_from < 0) return;
    a.released_from = -1;
    a.delay = delay;
    a.beat = -1;   // enter() puts it into the beat it left, at the phase it left
    a.rejoining = true;
    PED::SET_BLOCKING_OF_NON_TEMPORARY_EVENTS(a.ped, TRUE);
}
int Runtime::delay(int actor) const {
    return actor >= 0 && actor < actors() ? cast_[static_cast<size_t>(actor)].delay : 0;
}
int Runtime::ped(int actor) const { return actor >= 0 && actor < actors() ? cast_[static_cast<size_t>(actor)].ped : 0; }
const std::string& Runtime::actor_id(int actor) const { return spec_.actors[static_cast<size_t>(actor)].id; }
std::string Runtime::prop_id(int i) const {
    const Prop& p = props_[static_cast<size_t>(i)];
    return spec_.actors[static_cast<size_t>(p.owner)].id + ":" + p.model;
}
int Runtime::prop_owner(int i) const { return props_[static_cast<size_t>(i)].owner; }

void Runtime::sample_ped(int ped, EntitySample& out) {
    out = EntitySample{};
    if (ped == 0 || !ENTITY::DOES_ENTITY_EXIST(ped)) return;
    out.present = true;
    const Vector3 v = ENTITY::GET_ENTITY_COORDS(ped, TRUE);
    out.pos[0] = v.x; out.pos[1] = v.y; out.pos[2] = v.z;
    out.heading = ENTITY::GET_ENTITY_HEADING(ped);
    for (int b = 0; b < kSampleBones; ++b) {
        const Vector3 p = PED::GET_PED_BONE_COORDS(ped, kSampleBoneIds[static_cast<size_t>(b)], 0.f, 0.f, 0.f);
        out.bone[b][0] = p.x; out.bone[b][1] = p.y; out.bone[b][2] = p.z;
    }
}
void Runtime::sample_actor(int actor, EntitySample& out) const {
    const Actor& a = cast_[static_cast<size_t>(actor)];
    sample_ped(a.ped, out);
    if (out.present && a.released_from < 0 && a.scene >= 0) out.phase = PED::GET_SYNCHRONIZED_SCENE_PHASE(a.scene);
}
void Runtime::sample_prop(int i, EntitySample& out) const {
    out = EntitySample{};
    const Prop& p = props_[static_cast<size_t>(i)];
    if (p.object == 0 || !ENTITY::DOES_ENTITY_EXIST(p.object)) return;
    out.present = true;
    const Vector3 v = ENTITY::GET_ENTITY_COORDS(p.object, TRUE);
    out.pos[0] = v.x; out.pos[1] = v.y; out.pos[2] = v.z;
    out.heading = ENTITY::GET_ENTITY_HEADING(p.object);
    for (int b = 0; b < kSampleBones; ++b) { out.bone[b][0] = v.x; out.bone[b][1] = v.y; out.bone[b][2] = v.z; }
}

void Runtime::sample_track(int i, MapSample& out) const {
    out = MapSample{};
    const Track& t = tracks_[static_cast<size_t>(i)];
    if (t.object == 0 || !ENTITY::DOES_ENTITY_EXIST(t.object)) return;
    out.present = true;
    const Vector3 v = ENTITY::GET_ENTITY_COORDS(t.object, TRUE);
    out.pos[0] = v.x; out.pos[1] = v.y; out.pos[2] = v.z;
    ENTITY::GET_ENTITY_QUATERNION(t.object, &out.quat[0], &out.quat[1], &out.quat[2], &out.quat[3]);
}

int Runtime::cleared_left() const {
    int n = 0;
    for (const auto& c : clears_)
        n += static_cast<int>(c.boats ? boats_of(c.x, c.y, c.radius).size() : objects_of(c.model, c.x, c.y, c.radius).size());
    return n;
}

std::string Runtime::scan_objects(double x, double y, double radius) {
    static int handles[4096];
    const int n = worldGetAllObjects(handles, 4096);
    std::string s;
    char line[320];
    for (int i = 0; i < n; ++i) {
        const Object o = handles[i];
        if (!ENTITY::DOES_ENTITY_EXIST(o)) continue;
        const Vector3 v = ENTITY::GET_ENTITY_COORDS(o, TRUE);
        const double d = std::hypot(v.x - x, v.y - y);
        if (d > radius) continue;
        float q[4] = {0, 0, 0, 1};
        ENTITY::GET_ENTITY_QUATERNION(o, &q[0], &q[1], &q[2], &q[3]);
        std::snprintf(line, sizeof(line), "{\"model\":%u,\"pos\":[%.3f,%.3f,%.3f],\"quat\":[%.5f,%.5f,%.5f,%.5f],\"dist\":%.1f}\n",
                      static_cast<unsigned>(ENTITY::GET_ENTITY_MODEL(o)), v.x, v.y, v.z, q[0], q[1], q[2], q[3], d);
        s += line;
    }
    return s;
}

void Runtime::despawn() {
    for (auto& p : props_) {
        if (p.object == 0) continue;
        ENTITY::DETACH_ENTITY(p.object, TRUE, TRUE);
        ENTITY::SET_ENTITY_AS_MISSION_ENTITY(p.object, TRUE, TRUE);
        Object o = p.object;
        OBJECT::DELETE_OBJECT(&o);
    }
    props_.clear();
    tracks_.clear();   // the map's own: let go of, never deleted
    // the cleared models shown again (the map brings the objects back as it streams them in); REMOVE_MODEL_HIDE by
    // its hash: the SDK declares its coordinates Any, which would pass them as integers
    for (const auto& c : clears_) {
        if (c.boats) {
            const float r = static_cast<float>(c.radius), x = static_cast<float>(c.x), y = static_cast<float>(c.y), z = static_cast<float>(c.z);
            VEHICLE::SET_ALL_VEHICLE_GENERATORS_ACTIVE_IN_AREA(x - r, y - r, z - 50.0f, x + r, y + r, z + 50.0f, TRUE, FALSE);
            for (unsigned m : c.suppressed) VEHICLE::SET_VEHICLE_MODEL_IS_SUPPRESSED(m, FALSE);
        } else {
            invoke<Void>(0xD9E3006FB3CBD765, static_cast<float>(c.x), static_cast<float>(c.y), static_cast<float>(c.z), static_cast<float>(c.radius),
                         c.model, FALSE);
        }
    }
    clears_.clear();
    // Each scene exactly once: disposing an id twice could, once it had been
    // recycled, dispose something else.
    std::set<int> dead;
    for (const auto& e : retire_) dead.insert(e.first);
    for (const auto& s : shared_) dead.insert(s.scene);
    retire_.clear();
    shared_.clear();
    const Ped player = PLAYER::PLAYER_PED_ID();
    for (auto& a : cast_) {
        if (a.ped == 0) continue;
        if (a.scene >= 0) dead.insert(a.scene);
        if (a.ped == player) { hooks_.log("  CLEANUP: an actor is the player's body right now -- left in place"); continue; }
        AI::CLEAR_PED_TASKS_IMMEDIATELY(a.ped);
        ENTITY::SET_ENTITY_AS_MISSION_ENTITY(a.ped, TRUE, TRUE);
        Ped h = a.ped;
        PED::DELETE_PED(&h);
    }
    cast_.clear();
    for (int sc : dead) retire_scene(sc);
    for (auto& piece : pieces_) remove(piece);
}

}  // namespace diogenes::gta::stage
