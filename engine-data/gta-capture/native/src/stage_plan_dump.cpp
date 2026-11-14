// Where a stage spec puts its cast, tick by tick, without the game: the plan
// resolved from the spec and a calibration (a group's own, or the clip library's),
// on flat ground, written as one JSON line per tick. gta_capture/stage_check.py
// reads it and judges the script before anybody starts the game.
//
//   gta_stage_plan_dump <stage-spec.txt> <stage-calibration.txt> [<ground z>]
//
// A tick's line has the layout of the take's world telemetry (actors: id, beat,
// state.pos / h / phase / bones), under a schema of its own: this is what the
// plan says, not what a take logged.
//
// Positions and headings are the plan's own: beach-demo-003's recorded take AB
// has every actor within 0.2 mm and 0.00 degrees of them, all 120 s. Heights and
// bones are approximations, because the calibration holds a clip's pose relative
// to the root bone, not where that bone sits in the entity:
//   - the ground is flat, at <ground z> (the spec's frame z when not given);
//   - an entity stands kEntityAboveGroundM over it, whatever the clip (a clip
//     authored on the ground, the paparazzi's, carries its entity a metre over
//     the scene origin; a seated actor's is where the seat was measured);
//   - the lower foot of a pose is on the ground, the root bone as high over it
//     as the pose has it, and straight under the entity. On 003 the root bone
//     is up to 0.16 m to the side of the entity (a phone going away), mostly
//     0.03-0.06 m: bones are good to a hand's width, not to the millimetre.

#include "stage_plan.hpp"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <iostream>
#include <sstream>
#include <string>

using namespace diogenes::gta::stage;

namespace {

constexpr double kPi = 3.14159265358979323846;
constexpr double kEntityAboveGroundM = 0.99;   // stage_camera_paths.ROOT_ABOVE_SAND_M
constexpr double kFootBoneAboveSoleM = 0.08;   // stage_runtime's standing reference (kFootRefM)

bool read_file(const char* path, std::string& out) {
    std::ifstream f(path, std::ios::binary);
    if (!f) return false;
    std::stringstream s;
    s << f.rdbuf();
    out = s.str();
    return true;
}

std::string json_text(const std::string& s) {
    std::string o = "\"";
    for (const char ch : s) {
        if (ch == '"' || ch == '\\') { o += '\\'; o += ch; }
        else if (static_cast<unsigned char>(ch) < 0x20) o += ' ';
        else o += ch;
    }
    return o + "\"";
}

const char* kind_name(BeatKind k) {
    return k == BeatKind::Idle ? "idle" : k == BeatKind::Walk ? "walk" : k == BeatKind::Once ? "once" : "pair";
}

}  // namespace

int main(int argc, char** argv) {
    if (argc < 3 || argc > 4) {
        std::cerr << "usage: gta_stage_plan_dump <stage-spec.txt> <stage-calibration.txt> [<ground z>]\n";
        return 2;
    }
    std::string spec_text, cal_text, err;
    if (!read_file(argv[1], spec_text)) { std::cerr << "cannot read " << argv[1] << "\n"; return 3; }
    if (!read_file(argv[2], cal_text)) { std::cerr << "cannot read " << argv[2] << "\n"; return 3; }
    StageSpec spec;
    if (!(err = parse_stage_spec(spec_text, spec)).empty()) { std::cerr << "stage spec: " << err << "\n"; return 4; }
    Calibration cal;
    if (!(err = parse_calibration(cal_text, cal)).empty()) { std::cerr << "stage calibration: " << err << "\n"; return 5; }
    for (const auto& c : spec.clips)
        if (!cal.clip(c.id)) { std::cerr << "stage calibration: no clip " << c.id << "\n"; return 5; }
    double ground_z = spec.frame.z;
    if (argc == 4) {
        char* end = nullptr;
        ground_z = std::strtod(argv[3], &end);
        if (end == argv[3] || *end || !std::isfinite(ground_z)) { std::cerr << "ground z: not a number\n"; return 2; }
    }
    Plan plan;
    if (!(err = resolve_plan(spec, cal, [ground_z](double, double) { return ground_z; }, plan)).empty()) {
        std::cerr << "stage plan: " << err << "\n";
        return 6;
    }

    std::printf("{\"schema\":\"dm-gta-stage-plan-world/1\",\"header\":{\"frame\":[%.6f,%.6f,%.6f,%.6f],\"ticks\":%d,"
                "\"ground_z\":%.6f,\"sink\":%.6f,\"sets\":[",
                plan.frame.x, plan.frame.y, plan.frame.z, plan.frame.heading_deg, plan.ticks, ground_z, plan.sink);
    for (size_t i = 0; i < plan.sets.size(); ++i) {
        const SetPlacement& s = plan.sets[i];
        std::printf("%s{\"id\":%s,\"pos\":[%.6f,%.6f,%.6f],\"h\":%.6f,\"models\":[", i ? "," : "", json_text(s.id).c_str(), s.x, s.y, s.z,
                    s.heading);
        const SetSpec* piece = spec.set(s.id);   // the models it may be: what it is tells how much room it takes
        for (size_t m = 0; piece && m < piece->models.size(); ++m) std::printf("%s%s", m ? "," : "", json_text(piece->models[m]).c_str());
        std::printf("]}");
    }
    std::printf("],\"actors\":[");
    for (size_t a = 0; a < plan.actors.size(); ++a) {
        std::printf("%s{\"id\":%s,\"beats\":[", a ? "," : "", json_text(plan.actors[a].id).c_str());
        const auto& beats = plan.actors[a].beats;
        for (size_t i = 0; i < beats.size(); ++i) {
            const ResolvedBeat& r = beats[i];
            std::printf("%s{\"kind\":\"%s\",\"clip\":%s,\"start\":%d,\"ticks\":%d,\"entry_pop\":%.6f,\"blend_in\":%.6f}", i ? "," : "",
                        kind_name(r.kind), json_text(r.clip).c_str(), r.start, r.ticks, r.entry_pop, r.blend_in);
        }
        std::printf("]}");
    }
    std::printf("],\"notes\":[");
    for (size_t i = 0; i < plan.notes.size(); ++i) std::printf("%s%s", i ? "," : "", json_text(plan.notes[i]).c_str());
    std::printf("]}}\n");

    for (int k = 0; k < plan.ticks; ++k) {
        std::printf("{\"schema\":\"dm-gta-stage-plan-world/1\",\"tick\":%d,\"actors\":[", k);
        bool first = true;
        for (size_t a = 0; a < plan.actors.size(); ++a) {
            const TickOp op = tick_op(plan, cal, static_cast<int>(a), k);
            if (op.beat < 0) continue;   // the actor has no beat on this tick
            const ResolvedBeat& r = plan.actors[a].beats[static_cast<size_t>(op.beat)];
            const ClipCal& c = *cal.clip(r.clip);
            const int j = std::min(c.n - 1, static_cast<int>(op.phase * c.n));
            const size_t sj = static_cast<size_t>(j);
            const double sh = op.heading * kPi / 180.0;
            const double x = op.ox + c.root_x[sj] * std::cos(sh) - c.root_y[sj] * std::sin(sh);
            const double y = op.oy + c.root_x[sj] * std::sin(sh) + c.root_y[sj] * std::cos(sh);
            const double h = norm_heading(op.heading + c.yaw[sj]);
            const double eh = h * kPi / 180.0;
            const bool seated = spec.seat(plan.actors[a].id) && cal.seat(plan.actors[a].id) && cal.seat(plan.actors[a].id)->found;
            const double floor_z = ground_z - plan.sink;
            const double z = seated ? op.oz : floor_z + kEntityAboveGroundM;
            const double low = std::min(c.pose[static_cast<size_t>(c.pose_at(j, 4, 2))], c.pose[static_cast<size_t>(c.pose_at(j, 5, 2))]);
            const double root_z = seated ? op.oz : floor_z + kFootBoneAboveSoleM - low;
            std::printf("%s{\"id\":%s,\"beat\":%d,\"walking\":%d,\"state\":{\"pos\":[%.6f,%.6f,%.6f],\"h\":%.6f,\"phase\":%.6f,"
                        "\"bones\":[[%.6f,%.6f,%.6f]",
                        first ? "" : ",", json_text(plan.actors[a].id).c_str(), op.beat, op.walking ? 1 : 0, x, y, z, h,
                        static_cast<double>(op.phase), x, y, root_z);
            for (int b = 0; b < kPoseBones; ++b) {
                const double px = c.pose[static_cast<size_t>(c.pose_at(j, b, 0))], py = c.pose[static_cast<size_t>(c.pose_at(j, b, 1))];
                std::printf(",[%.6f,%.6f,%.6f]", x + px * std::cos(eh) - py * std::sin(eh), y + px * std::sin(eh) + py * std::cos(eh),
                            root_z + c.pose[static_cast<size_t>(c.pose_at(j, b, 2))]);
            }
            std::printf("]}}");
            first = false;
        }
        std::printf("]}\n");
    }
    return 0;
}
