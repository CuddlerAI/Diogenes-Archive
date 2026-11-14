#include "stage_take.hpp"

#include <main.h>
#include <natives.h>
#include <windows.h>

#include <algorithm>
#include <cmath>
#include <condition_variable>
#include <cstdio>
#include <deque>
#include <mutex>
#include <sstream>
#include <thread>

namespace {
double perf_now_ms() {
    static const double per_ms = [] { LARGE_INTEGER f; QueryPerformanceFrequency(&f); return static_cast<double>(f.QuadPart) / 1000.0; }();
    LARGE_INTEGER c; QueryPerformanceCounter(&c);
    return static_cast<double>(c.QuadPart) / per_ms;
}
}  // namespace

namespace diogenes::gta::stage {

namespace {

constexpr double kPi = 3.14159265358979323846;
// The game camera's follow-ped view modes (CAM::GET_FOLLOW_PED_CAM_VIEW_MODE): third person
// (medium), cinematic, first person.
constexpr int kThirdPersonView = 1, kCinematicView = 3, kFirstPersonView = 4;
// Standing, the program turns the played body once it is this far off where it is to face, and
// again once that has moved this much, or the turn ran out of time (hold_view).
constexpr double kTurnStartDeg = 4.0, kRetargetDeg = 3.0;
constexpr int kTurnTimeoutMs = 3000;
// Controls (GTA V input ids).
constexpr int kLookLR = 1, kLookUD = 2, kMoveLR = 30, kMoveUD = 31;
constexpr int kSprint = 21, kJump = 22, kEnter = 23, kAttack = 24, kAim = 25, kMeleeLight = 140, kMeleeHeavy = 141;
// Held keys are logged for the whole physical range a person uses in a take:
// mouse buttons, editing keys, digits, letters, modifiers. Combinations are kept.
bool LoggedKey(int vk) {
    return (vk >= 0x01 && vk <= 0x06) || (vk >= 0x08 && vk <= 0x2F) || (vk >= 0x30 && vk <= 0x39) || (vk >= 0x41 && vk <= 0x5A) ||
           (vk >= 0xA0 && vk <= 0xA5);
}
// The requirement's p95 limits for a free-moving camera (#958 §1), flown at nine
// tenths of them: then not even the worst frame reaches them.
constexpr double kMargin = 0.9;
constexpr double kMaxSpeed = 6.0 * kMargin, kMaxAccel = 8.0 * kMargin, kMaxJerk = 20.0 * kMargin;          // m/s, m/s^2, m/s^3
constexpr double kMaxRate = 90.0 * kMargin, kMaxAngAccel = 150.0 * kMargin, kMaxAngJerk = 600.0 * kMargin;   // deg/s, /s^2, /s^3
// A new scene's task holds within two frames (the check used to wait two frames);
// at the recorder's 30 fps cap that is two ticks.
constexpr int kCheckAfterTicks = 2;
// How long a "watch" reaction watches before the watcher carries on (the user,
// 2026-09-28: watching to the end, everybody stood still past C's last reaction).
constexpr int kWatchTicks = 5 * kTickHz;

std::string num(double v) {
    char b[40];
    std::snprintf(b, sizeof(b), "%.9g", v);
    return b;
}
// The delivery's `time` is the real system time a frame was played (requirement
// rev 630 L213), so every tick logs the local wall clock it was posed at.
std::string local_wall_now() {
    FILETIME utc{}, local{};
    SYSTEMTIME st{};
    GetSystemTimePreciseAsFileTime(&utc);
    if (!FileTimeToLocalFileTime(&utc, &local) || !FileTimeToSystemTime(&local, &st)) return "";
    char b[32];
    std::snprintf(b, sizeof(b), "%04u-%02u-%02u %02u:%02u:%02u.%03u", st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute,
                  st.wSecond, st.wMilliseconds);
    return b;
}
std::string vec3(const float* p) { return "[" + num(p[0]) + "," + num(p[1]) + "," + num(p[2]) + "]"; }
std::string vec3(double x, double y, double z) { return "[" + num(x) + "," + num(y) + "," + num(z) + "]"; }

std::string entity_json(const EntitySample& e, bool bones) {
    if (!e.present) return "null";
    std::string s = "{\"pos\":" + vec3(e.pos) + ",\"h\":" + num(e.heading);
    if (e.phase >= 0) s += ",\"phase\":" + num(e.phase);
    if (bones) {
        s += ",\"bones\":[";
        for (int b = 0; b < kSampleBones; ++b) s += (b ? "," : "") + vec3(e.bone[b]);
        s += "]";
    }
    return s + "}";
}

// Program input goes where a person's does: the keyboard and the mouse, through
// SendInput -- and only while the game's own window is in front, so not one key
// ever lands in another program.
bool game_in_front() {
    HWND w = GetForegroundWindow();
    DWORD pid = 0;
    if (w != nullptr) GetWindowThreadProcessId(w, &pid);
    return w != nullptr && pid == GetCurrentProcessId();
}
// The program's keys go to the game through SendInput -- on a thread of their own.
// Called on the game's thread, a keyboard event through SendInput stalled it 30-66 ms
// on the capture machine (other software's low-level keyboard hooks run inside the
// call), once per change of the held keys: take AB of beach-demo-003, whose free
// camera changes keys every few seconds, dropped a frame at each of them and read
// 28-29 fps in those seconds (2026-09-30, measured frame by frame). Mouse events
// never stalled and stay on the game's thread. One queue keeps the keys in order:
// a key's release must not overtake its press.
class KeyPump {
public:
    void push(const INPUT& in) {
        {
            std::lock_guard<std::mutex> lock(m_);
            if (!worker_.joinable()) { stop_ = false; worker_ = std::thread([this] { run(); }); }
            q_.push_back(in);
        }
        cv_.notify_one();
    }
    // every queued event sent, then the thread gone: at the end of a take
    void drain_and_stop() {
        std::unique_lock<std::mutex> lock(m_);
        if (!worker_.joinable()) return;
        done_.wait(lock, [&] { return q_.empty() && !sending_; });
        stop_ = true;
        lock.unlock();
        cv_.notify_one();
        worker_.join();
    }
    ~KeyPump() { if (worker_.joinable()) { { std::lock_guard<std::mutex> lock(m_); stop_ = true; } cv_.notify_one(); worker_.detach(); } }
private:
    void run() {
        std::unique_lock<std::mutex> lock(m_);
        for (;;) {
            cv_.wait(lock, [&] { return stop_ || !q_.empty(); });
            if (q_.empty()) return;   // stopped, nothing left
            INPUT in = q_.front();
            q_.pop_front();
            sending_ = true;
            lock.unlock();
            SendInput(1, &in, sizeof(INPUT));
            lock.lock();
            sending_ = false;
            if (q_.empty()) done_.notify_all();
        }
    }
    std::thread worker_;
    std::mutex m_;
    std::condition_variable cv_, done_;
    std::deque<INPUT> q_;
    bool stop_ = false, sending_ = false;
};
KeyPump g_key_pump;

void send_key(WORD vk, bool down) {
    INPUT in{};
    in.type = INPUT_KEYBOARD;
    in.ki.wVk = vk;
    in.ki.wScan = static_cast<WORD>(MapVirtualKeyW(vk, MAPVK_VK_TO_VSC));
    in.ki.dwFlags = down ? 0 : KEYEVENTF_KEYUP;
    g_key_pump.push(in);
}
void send_mouse(int dx, int dy) {
    INPUT in{};
    in.type = INPUT_MOUSE;
    in.mi.dx = dx;
    in.mi.dy = dy;
    in.mi.dwFlags = MOUSEEVENTF_MOVE;
    SendInput(1, &in, sizeof(INPUT));
}
constexpr WORD kRobotKeys[6] = {'W', 'A', 'S', 'D', 'Q', 'E'};

double wrap180(double d) { return std::fmod(d + 540.0, 360.0) - 180.0; }
double lerp(double a, double b, double w) { return a + (b - a) * w; }
double lerp_deg(double a, double b, double w) { return a + wrap180(b - a) * w; }

}  // namespace

std::string json_str(const std::string& s) {
    std::string o = "\"";
    for (const unsigned char c : s) {
        switch (c) {
        case '"': o += "\\\""; break;
        case '\\': o += "\\\\"; break;
        case '\n': o += "\\n"; break;
        case '\r': o += "\\r"; break;
        case '\t': o += "\\t"; break;
        default:
            if (c < 0x20) { char b[8]; std::snprintf(b, sizeof(b), "\\u%04x", c); o += b; }
            else o += static_cast<char>(c);
        }
    }
    return o + "\"";
}

Take::Take(Runtime& rt, const StageSpec& spec, const Calibration& cal, const Plan& plan, TakeConfig cfg, Hooks hooks)
    : rt_(rt), spec_(spec), cal_(cal), plan_(plan), cfg_(std::move(cfg)), hooks_(std::move(hooks)) {}

Take::~Take() {
    if (!ended_) end();
}

int Take::controlled_ped() const {
    if (cfg_.kind == TakeKind::D) return seventh_;
    if (cfg_.kind == TakeKind::C && taken_) return rt_.ped(takeover_index_);
    return 0;
}
std::string Take::controlled_id() const {
    if (cfg_.kind == TakeKind::D) return "seventh";
    if (cfg_.kind == TakeKind::C && taken_) return rt_.actor_id(takeover_index_);
    return "";
}

std::string Take::prepare() {
    own_body_ = PLAYER::PLAYER_PED_ID();
    saved_view_mode_ = CAM::GET_FOLLOW_PED_CAM_VIEW_MODE();
    // Once the player plays somebody else, the story character is an ordinary
    // ped the game may clear away: keep it (taking it from no script that owns it).
    if (cfg_.kind != TakeKind::AB && !ENTITY::IS_ENTITY_A_MISSION_ENTITY(own_body_))
        ENTITY::SET_ENTITY_AS_MISSION_ENTITY(own_body_, TRUE, FALSE);
    if (cfg_.kind == TakeKind::C) {
        for (size_t i = 0; i < spec_.actors.size(); ++i)
            if (spec_.actors[i].id == cfg_.takeover_actor) takeover_index_ = static_cast<int>(i);
        if (takeover_index_ < 0) return "take C: no actor named " + cfg_.takeover_actor;
        if (cfg_.takeover_tick < 0 || cfg_.takeover_tick >= plan_.ticks) return "take C: the takeover tick is outside the take";
    }
    if (cfg_.kind != TakeKind::AB && spec_.stance == "clip") {   // the stance clip, loaded before the recorder
        char* dict = const_cast<char*>(spec_.stance_dict.c_str());
        STREAMING::REQUEST_ANIM_DICT(dict);
        for (int f = 0; f < 300 && !STREAMING::HAS_ANIM_DICT_LOADED(dict); ++f) hooks_.frame();
        if (!STREAMING::HAS_ANIM_DICT_LOADED(dict)) return "stance: animation dictionary " + spec_.stance_dict + " did not load";
        if (!(ENTITY::_GET_ANIM_DURATION(dict, const_cast<char*>(spec_.stance_clip.c_str())) > 0.0f))
            return "stance: clip " + spec_.stance_dict + " / " + spec_.stance_clip + " has no duration -- not in its dictionary";
    }
    if (cfg_.kind != TakeKind::AB) hooks_.log("  STANCE: the played person stands " + spec_.stance +
                                              (spec_.stance == "clip" ? " " + spec_.stance_dict + " / " + spec_.stance_clip : ""));
    const std::string take = cfg_.kind == TakeKind::AB ? "AB" : cfg_.kind == TakeKind::C ? "C" : "D";
    if (const RobotSpec* r = spec_.robot(take)) {   // a program plays this take instead of a person
        robot_ = true;
        const double duration = plan_.ticks / static_cast<double>(kTickHz);
        if (cfg_.kind != TakeKind::AB) {   // how the program turns a standing body (the customer, 2026-10-10)
            const std::string bad = robot::parse_turn(cfg_.turn, turn_style_);
            if (!bad.empty()) return "take " + take + ": " + bad;
            hooks_.log("  ROBOT: a standing body turns " + robot::turn_text(turn_style_));
        }
        if (cfg_.kind == TakeKind::AB) {
            flyer_ = std::make_unique<robot::Flyer>(r->seed, duration);
        } else if (!cfg_.route.empty()) {
            // C on a fixed route (the user, 2026-09-28: no dice), from the takeover; D's seventh person on one
            // too (2026-10-16, the walk turn style: a route's taps turn it to look), from tick 0
            std::vector<robot::RouteStep> steps;
            const std::string bad = robot::parse_route(cfg_.route, steps);
            if (!bad.empty()) return "take " + take + ": " + bad;
            for (const robot::RouteStep& s : steps) {
                bool known = s.look == "-";
                for (const ActorSpec& a : spec_.actors) known = known || a.id == s.look;
                if (!known || (cfg_.kind == TakeKind::C && s.look == cfg_.takeover_actor))
                    return "take " + take + ": the route looks at " + s.look + ", who is nobody to look at";
            }
            const double from_s = cfg_.kind == TakeKind::C ? cfg_.takeover_tick / static_cast<double>(kTickHz) : 0.0;
            router_ = std::make_unique<robot::RouteWalker>(steps, from_s, robot::WalkRoom{}, turn_style_);
            hooks_.log("  ROBOT: a program plays take " + take + " on a fixed route of " + std::to_string(steps.size()) + " spots: " + cfg_.route);
        } else {
            // the walk style leaves a standing body to the keys: a stroll would never turn to watch the cast
            if (turn_style_.mode == robot::TurnMode::Walk) return "take " + take + ": turn walk needs a route (its taps turn the standing person)";
            std::vector<robot::Reach> reaches;   // what a changed walk may set off; an unchanged one keeps clear of all of it
            for (const RuleSpec& rule : spec_.rules) if (rule.when == "near") reaches.push_back({rule.subject, rule.metres});
            walker_ = std::make_unique<robot::Walker>(r->seed, r->mode == "changed", duration, reaches, robot::WalkRoom{}, turn_style_);
        }
        if (!router_) hooks_.log("  ROBOT: a program plays take " + take + " (" + r->mode + ", seed " + std::to_string(r->seed) + ")");
    }
    // The player's own body waits hidden and frozen, out of every shot.
    double px = 0, py = 0;
    spec_.frame.to_world(cfg_.park_x, cfg_.park_y, px, py);
    const double pz = rt_.ground(px, py);
    ENTITY::FREEZE_ENTITY_POSITION(own_body_, TRUE);
    ENTITY::SET_ENTITY_COORDS(own_body_, static_cast<float>(px), static_cast<float>(py), static_cast<float>(pz), FALSE, FALSE, FALSE, TRUE);
    ENTITY::SET_ENTITY_VISIBLE(own_body_, FALSE, FALSE);
    ENTITY::SET_ENTITY_COLLISION(own_body_, FALSE, FALSE);
    ENTITY::SET_ENTITY_INVINCIBLE(own_body_, TRUE);

    const std::string err = rt_.spawn(spec_, plan_, cal_);
    if (!err.empty()) return err;
    stats_.cleared = rt_.cleared();
    rt_.enter(plan_, cal_, 0);
    hooks_.frame(); hooks_.frame();
    const std::string bad = rt_.verify(plan_, 0);
    if (!bad.empty()) hooks_.log("  SCENE at tick 0: " + bad);
    rt_.pose(plan_, cal_, 0);

    if (cfg_.kind == TakeKind::AB) {
        double cx = 0, cy = 0;
        spec_.frame.to_world(cfg_.cam_x, cfg_.cam_y, cx, cy);
        free_.px.x = cx; free_.py.x = cy; free_.pz.x = rt_.ground(cx, cy) + cfg_.cam_h;
        free_.yaw.x = spec_.frame.heading_to_world(cfg_.cam_heading);
        free_.pitch.x = -5.0; free_.fov = cfg_.cam_fov;
        free_.cam = CAM::CREATE_CAM(const_cast<char*>("DEFAULT_SCRIPTED_CAMERA"), TRUE);
        if (free_.cam == 0) return "take AB: no scripted camera for the free camera";
        CAM::SET_CAM_COORD(free_.cam, static_cast<float>(free_.px.x), static_cast<float>(free_.py.x), static_cast<float>(free_.pz.x));
        CAM::SET_CAM_ROT(free_.cam, static_cast<float>(free_.pitch.x), 0.0f, static_cast<float>(free_.yaw.x), 2);
        CAM::SET_CAM_FOV(free_.cam, static_cast<float>(free_.fov));
        CAM::SET_CAM_ACTIVE(free_.cam, TRUE);
        CAM::RENDER_SCRIPT_CAMS(TRUE, FALSE, 0, TRUE, FALSE);
    } else if (cfg_.kind == TakeKind::C) {
        // Until the takeover the hidden body rides on the actor, so the game's own
        // camera already follows the person the player will become.
        const int k = rt_.ped(takeover_index_);
        if (k == 0) return "take C: the actor to take over did not spawn";
        ENTITY::FREEZE_ENTITY_POSITION(own_body_, FALSE);
        ENTITY::ATTACH_ENTITY_TO_ENTITY(own_body_, k, 0, 0.0f, 0.0f, 0.0f, 0.0f, 0.0f, 0.0f, FALSE, FALSE, FALSE, TRUE, 2, TRUE);
    } else {
        double sx = 0, sy = 0;
        spec_.frame.to_world(cfg_.seventh_x, cfg_.seventh_y, sx, sy);
        const double sz = rt_.ground(sx, sy);
        const Hash h = GAMEPLAY::GET_HASH_KEY(const_cast<char*>(cfg_.seventh_model.c_str()));
        STREAMING::REQUEST_MODEL(h);
        for (int f = 0; f < 300 && !STREAMING::HAS_MODEL_LOADED(h); ++f) hooks_.frame();
        if (!STREAMING::HAS_MODEL_LOADED(h)) return "take D: the seventh person's model " + cfg_.seventh_model + " did not load";
        seventh_ = PED::CREATE_PED(4, h, static_cast<float>(sx), static_cast<float>(sy), static_cast<float>(sz + 1.0),
                                   static_cast<float>(spec_.frame.heading_to_world(cfg_.seventh_heading)), FALSE, TRUE);
        STREAMING::SET_MODEL_AS_NO_LONGER_NEEDED(h);   // the ped keeps it loaded
        if (!ENTITY::DOES_ENTITY_EXIST(seventh_)) return "take D: the seventh person did not spawn";
        PED::SET_PED_DEFAULT_COMPONENT_VARIATION(seventh_);
        // The player plays the seventh person; the story character is never re-modelled.
        PLAYER::CHANGE_PLAYER_PED(PLAYER::PLAYER_ID(), seventh_, FALSE, FALSE);
        stats_.seventh_ok = PLAYER::PLAYER_PED_ID() == seventh_;
        if (!stats_.seventh_ok) return "take D: the player did not take the seventh person";
        for (int f = 0; f < 60; ++f) { hold_view(); hooks_.frame(); }   // lands and stands before the recorder
        stats_.view_mode_restores = 0;   // what the take holds is counted from the recorder on
    }
    return "";
}

std::string Take::begin(long long perf0_ms, const std::string& source_dir) {
    perf0_ = perf0_ms;
    last_game_ms_ = perf0_ms;
    world_path_ = source_dir + "\\master-world-telemetry.jsonl";
    input_path_ = source_dir + "\\input-camera-log.jsonl";
    rules_path_ = source_dir + "\\rule-events.jsonl";
    world_.open(world_path_, std::ios::binary | std::ios::trunc);
    input_.open(input_path_, std::ios::binary | std::ios::trunc);
    rules_.open(rules_path_, std::ios::binary | std::ios::trunc);
    if (!world_ || !input_ || !rules_) return "cannot open the take's logs in " + source_dir;
    scan_dir_ = source_dir;
    write_scan("map-scan-begin.jsonl");   // what of the map's moves: the objects round the stage now and at the end
    if (cfg_.kind != TakeKind::C) control_ms_ = perf0_ms;   // AB and D are played from tick 0; C from its takeover
    if (cfg_.kind == TakeKind::D)
        write_rule_event(0, "seventh", "take", "seventh", 0, {{"seventh", "player"}},
                         std::string("第 0 秒起由") + (robot_ ? "程序键鼠" : "玩家") + "操控新增的第 7 人（" + cfg_.seventh_model + "）");
    return "";
}

// Every object within kScanRadiusM of the stage (the pier's ferris wheel is ~90 m off): to find what a track line should
// name, and -- begin against end -- which of them moved or turned during the take.
constexpr double kScanRadiusM = 200.0;
void Take::write_scan(const std::string& name) {
    if (scan_dir_.empty()) return;
    std::ofstream f(scan_dir_ + "\\" + name, std::ios::binary | std::ios::trunc);
    f << Runtime::scan_objects(spec_.frame.x, spec_.frame.y, kScanRadiusM);
    if (!f) hooks_.log("  map scan: could not write " + name);
}

void Take::write_rule_event(int tick, const std::string& rule, const std::string& when, const std::string& subject, double value,
                            const std::vector<std::pair<std::string, std::string>>& affected, const std::string& say) {
    std::string a = "[";
    for (size_t i = 0; i < affected.size(); ++i)
        a += (i ? "," : "") + std::string("{\"actor\":") + json_str(affected[i].first) + ",\"action\":" + json_str(affected[i].second) + "}";
    a += "]";
    rules_ << "{\"schema\":\"dm-gta-rule-events/1\",\"tick\":" << tick << ",\"t_s\":" << num(tick / double(kTickHz)) << ",\"rule\":" << json_str(rule)
           << ",\"when\":" << json_str(when) << ",\"subject\":" << json_str(subject) << ",\"value\":" << num(value) << ",\"affected\":" << a
           << ",\"say\":" << json_str(say) << "}\n";
    rules_.flush();
    if (!rules_) stats_.logs_ok = false;
}

void Take::react(int actor, const std::string& action, int by, int k) {
    if (actor < 0 || rt_.released(actor)) return;
    const int p = rt_.ped(actor);
    // where it stood and faced, and the plan's tick it was playing: where carry_on picks up
    Watcher w;
    w.actor = actor;
    w.until = k + kWatchTicks;
    w.plan_tick = k - rt_.delay(actor);
    if (p != 0) {
        const Vector3 at = ENTITY::GET_ENTITY_COORDS(p, TRUE);
        w.x = at.x; w.y = at.y; w.z = at.z;
        w.heading = ENTITY::GET_ENTITY_HEADING(p);
    }
    rt_.release(actor, k, action == "watch");   // a watcher stops as it turns, out of its stride
    ++stats_.releases;
    if (p == 0) return;
    if (action == "flee") AI::TASK_SMART_FLEE_PED(p, by, 30.0f, static_cast<Any>(-1), FALSE, FALSE);
    else if (action == "handsup") AI::TASK_HANDS_UP(p, -1, by, static_cast<Any>(-1), static_cast<Any>(0));
    else if (action == "cower") AI::TASK_COWER(p, -1);
    else if (action == "fight") AI::TASK_COMBAT_PED(p, by, 0, 16);
    else if (action == "watch") {
        watching_.push_back(w);
        AI::TASK_TURN_PED_TO_FACE_ENTITY(p, by, -1);
        AI::TASK_LOOK_AT_ENTITY(p, by, -1, 0, 2);
    }
}

// Back to what the watcher was doing, exactly as it did it: into the plan again at the
// tick it left, as far behind the plan as it watched (Runtime::rejoin) -- the pair chats
// on, the tourist's pictures and walks go on, the jogger runs on in her own stride (the
// game's own tasks made a frozen photographer and a jogger who looked to be fleeing,
// 2026-09-28). Pushed aside or turned while watching, it first walks back to its spot
// and turns the way it faced, so the plan takes it up where it stands.
void Take::carry_on(const Watcher& w, int k) {
    const int p = rt_.ped(w.actor);
    if (p == 0 || !ENTITY::DOES_ENTITY_EXIST(p) || PED::IS_PED_INJURED(p)) return;
    AI::TASK_CLEAR_LOOK_AT(p);
    AI::CLEAR_PED_TASKS(p);
    PED::SET_BLOCKING_OF_NON_TEMPORARY_EVENTS(p, TRUE);   // its own thing, not the game's idea of a response
    Routine r;
    r.w = w;
    const Vector3 at = ENTITY::GET_ENTITY_COORDS(p, TRUE);
    if (std::hypot(at.x - w.x, at.y - w.y) > 0.25) r.steps.push_back(Routine::Back);
    r.steps.push_back(Routine::Turn);
    // a pair's one scene cannot run late for one half: stopped in a pair beat, it stays off the plan
    const TickOp op = tick_op(plan_, cal_, w.actor, w.plan_tick);
    const bool pair = op.beat >= 0 && plan_.actors[static_cast<size_t>(w.actor)].beats[static_cast<size_t>(op.beat)].kind == BeatKind::Pair;
    if (!pair) r.steps.push_back(Routine::Rejoin);
    routines_.push_back(r);
    start_step(routines_.back(), k);
}

void Take::start_step(Routine& r, int k) {
    const int p = rt_.ped(r.w.actor);
    switch (r.steps[r.at]) {
    case Routine::Back:
        AI::TASK_GO_STRAIGHT_TO_COORD(p, static_cast<float>(r.w.x), static_cast<float>(r.w.y), static_cast<float>(r.w.z), 1.0f, -1,
                                      static_cast<float>(r.w.heading), 0.3f);
        r.until = k + 4 * kTickHz;
        break;
    case Routine::Turn:
        AI::TASK_ACHIEVE_HEADING(p, static_cast<float>(r.w.heading), 3000);
        r.until = k + 3 * kTickHz;
        break;
    case Routine::Rejoin:
        rt_.rejoin(r.w.actor, k - r.w.plan_tick);
        r.finished = true;
        break;
    }
}

// On to the routine's next step once this one is there (or has had its time).
void Take::advance_routine(Routine& r, int k) {
    if (r.finished) return;
    const int p = rt_.ped(r.w.actor);
    if (p == 0 || !ENTITY::DOES_ENTITY_EXIST(p) || PED::IS_PED_INJURED(p)) { r.finished = true; return; }
    bool done = k >= r.until;
    if (r.steps[r.at] == Routine::Back) {
        const Vector3 at = ENTITY::GET_ENTITY_COORDS(p, TRUE);
        done = done || std::hypot(at.x - r.w.x, at.y - r.w.y) < 0.15;
    } else if (r.steps[r.at] == Routine::Turn) {
        const double off = std::fabs(std::fmod(ENTITY::GET_ENTITY_HEADING(p) - r.w.heading + 540.0, 360.0) - 180.0);
        done = done || off < 10.0;
    }
    if (!done) return;
    if (++r.at >= r.steps.size()) { r.finished = true; return; }
    start_step(r, k);
}

void Take::take_over(int k) {
    const int k_ped = rt_.ped(takeover_index_);
    stats_.takeover_tick = k;
    if (k_ped == 0) { stats_.last_problem = "the actor to take over is gone"; return; }
    ENTITY::DETACH_ENTITY(own_body_, TRUE, FALSE);
    double px = 0, py = 0;
    spec_.frame.to_world(cfg_.park_x, cfg_.park_y, px, py);
    ENTITY::SET_ENTITY_COORDS(own_body_, static_cast<float>(px), static_cast<float>(py), static_cast<float>(rt_.ground(px, py)), FALSE, FALSE, FALSE, TRUE);
    ENTITY::FREEZE_ENTITY_POSITION(own_body_, TRUE);
    rt_.release(takeover_index_, k);
    PLAYER::CHANGE_PLAYER_PED(PLAYER::PLAYER_ID(), k_ped, FALSE, FALSE);
    taken_ = stats_.takeover_ok = PLAYER::PLAYER_PED_ID() == k_ped;
    if (taken_) hold_view();
    control_ms_ = last_game_ms_;
    write_rule_event(k, "takeover", "take", cfg_.takeover_actor, 0, {{cfg_.takeover_actor, "player"}},
                     "第 " + std::to_string(k / kTickHz) + " 秒玩家接管 " + cfg_.takeover_actor + "，此后此人由" +
                         (robot_ ? "程序" : "真人") + "键鼠操控");
}

// The played person: in the game's third person, walking, and -- when a program plays it --
// turned after its view while it stands. The customer (2026-10-09): turning on the spot the
// first-person view went round first and the body 1-2 s later; the view must go with the face.
// The game's own first person turns the body with the mouse, but hides the person's head, and
// the replay keeps it hidden: in the export's other cameras the person had no head (beach-demo-003
// D, 2026-10-09; neither a scripted camera rendering instead -- the game camera froze -- nor the
// head-hiding reset flag cleared each frame kept it). In its third person the mouse turns only the
// camera and the body turns once a key goes down, so standing, the program turns the body with the
// game's own turning on the spot -- in one go, to where the program that plays means it to face
// (robot::Input::body_to), its camera following -- and hands it back to the keys as they go down.
// (Turned after the camera, a little at a time, it went in fits and the view led it by 45 degrees.)
// The game walks on W in its third person; held to a walk all the same, for one frame at a time.
// Turn "glide" (2026-10-13): the game's turn has no speed, so the program sets the standing body's heading
// itself every frame, where robot::Turner glides it -- at most the group's deg/s, eased in and out -- and
// the camera follows as before. Turn "steps": the game's own turn, to a step of it at a time (robot::Turner).
// The played person standing with the keys at rest (spec stance, 2026-10-18): GTA's own idle sways the head 5-8 cm
// sideways every second or two, and the first person rides the head (beach-demo-113). Calm: the game's ambient,
// gesture and look-at animation off, every frame (a task change may put them back). Clip: a declared loop played
// once the person has stood still kStanceAfterFrames, blended in and out over half a second, stopped the frame the
// keys go down so the game's own walk takes it from there.
static constexpr int kStanceAfterFrames = 10;
void Take::hold_stance(int me) {
    if (spec_.stance == "game") return;
    PED::SET_PED_CAN_PLAY_AMBIENT_ANIMS(me, FALSE);
    PED::SET_PED_CAN_PLAY_AMBIENT_BASE_ANIMS(me, FALSE);
    PED::SET_PED_CAN_PLAY_GESTURE_ANIMS(me, FALSE);
    PED::SET_PED_CAN_HEAD_IK(me, FALSE);
    if (spec_.stance != "clip") return;
    char* dict = const_cast<char*>(spec_.stance_dict.c_str());
    char* name = const_cast<char*>(spec_.stance_clip.c_str());
    const bool still = !held_.any() && ENTITY::GET_ENTITY_SPEED(me) < 0.15f;
    still_frames_ = still ? still_frames_ + 1 : 0;
    const bool playing = ENTITY::IS_ENTITY_PLAYING_ANIM(me, dict, name, 3) != 0;
    if (still_frames_ >= kStanceAfterFrames && !playing) {
        AI::TASK_PLAY_ANIM(me, dict, name, 2.0f, -2.0f, -1, 1, 0.0f, FALSE, FALSE, FALSE);
        ++stats_.stance_starts;
    } else if (!still && playing) {
        AI::STOP_ANIM_TASK(me, dict, name, -2.0f);
    }
}

void Take::hold_view() {
    const int me = controlled_ped();
    if (me == 0) return;
    PED::SET_PED_MAX_MOVE_BLEND_RATIO(me, 1.0f);
    hold_stance(me);
    const int mode = CAM::GET_FOLLOW_PED_CAM_VIEW_MODE();
    if (mode == kFirstPersonView || mode == kCinematicView) {
        CAM::SET_FOLLOW_PED_CAM_VIEW_MODE(kThirdPersonView);
        ++stats_.view_mode_restores;
    }
    if (!robot_ || robot_stopped_) return;   // a person turns the body with the keys
    if (held_.any()) {   // walking: the keys turn it
        if (turning_) { AI::CLEAR_PED_TASKS(me); turning_ = false; }
        return;
    }
    if (std::isnan(body_to_)) return;
    const double off = wrap180(body_to_ - ENTITY::GET_ENTITY_HEADING(me));
    if (turn_style_.mode == robot::TurnMode::Glide) {
        if (turning_) { AI::CLEAR_PED_TASKS(me); turning_ = false; }   // no game turn under the glide
        if (std::fabs(off) < 0.01) return;
        ENTITY::SET_ENTITY_HEADING(me, static_cast<float>(body_to_));
        PED::SET_PED_DESIRED_HEADING(me, static_cast<float>(body_to_));   // or the game turns it back
        ++stats_.glide_frames;
        return;
    }
    const long long now = last_game_ms_;
    const bool same = turning_ && std::fabs(wrap180(body_to_ - turn_to_)) < kRetargetDeg;
    if (std::fabs(off) < kTurnStartDeg || (same && now - turn_ms_ < kTurnTimeoutMs)) return;
    AI::TASK_ACHIEVE_HEADING(me, static_cast<float>(body_to_), kTurnTimeoutMs);
    if (!same) ++stats_.body_turns;
    turning_ = true;
    turn_to_ = body_to_;
    turn_ms_ = now;
}

void Take::evaluate_rules(int k) {
    const int me = controlled_ped();
    // Nothing the person did yet, nothing to answer: at C's takeover the taken woman
    // stood 0.93 m from her partner and "near" fired before the first key (2026-09-24).
    if (me == 0 || !acted_) return;
    for (const RuleSpec& r : spec_.rules) {
        if (fired_.count(r.id)) continue;
        int subject = -1;
        for (size_t i = 0; i < spec_.actors.size(); ++i) if (spec_.actors[i].id == r.subject) subject = static_cast<int>(i);
        const int sp = rt_.ped(subject);
        if (sp == 0 || sp == me) continue;
        double value = 0;
        bool hit = false;
        if (r.when == "near") {
            const Vector3 a = ENTITY::GET_ENTITY_COORDS(me, TRUE), b = ENTITY::GET_ENTITY_COORDS(sp, TRUE);
            value = std::sqrt((a.x - b.x) * (a.x - b.x) + (a.y - b.y) * (a.y - b.y) + (a.z - b.z) * (a.z - b.z));
            if (value >= r.metres) near_armed_.insert(r.id);   // out of reach: coming closer now is an approach
            hit = value < r.metres && near_armed_.count(r.id) != 0;
        } else if (r.when == "touch") {
            hit = ENTITY::IS_ENTITY_TOUCHING_ENTITY(me, sp) != 0;
        } else if (r.when == "aim") {
            hit = PLAYER::IS_PLAYER_FREE_AIMING_AT_ENTITY(PLAYER::PLAYER_ID(), sp) != 0;
        } else if (r.when == "damage") {
            hit = ENTITY::HAS_ENTITY_BEEN_DAMAGED_BY_ENTITY(sp, me, TRUE) != 0;
        }
        if (!hit) continue;
        fired_.insert(r.id);
        ++stats_.rule_firings;
        std::vector<std::pair<std::string, std::string>> reacted;   // never the person's own body
        for (const auto& t : r.then) {
            for (size_t i = 0; i < spec_.actors.size(); ++i)
                if (spec_.actors[i].id == t.first && rt_.ped(static_cast<int>(i)) != me) {
                    react(static_cast<int>(i), t.second, me, k);
                    reacted.push_back(t);
                }
        }
        write_rule_event(k, r.id, r.when, r.subject, value, reacted, r.say);
    }
}

void Take::accumulate_input() {
    for (int vk = 1; vk < 0xFF; ++vk)
        if (LoggedKey(vk) && (GetAsyncKeyState(vk) & 0x8000) != 0) keys_.insert(vk);
    mouse_dx_ += CONTROLS::GET_DISABLED_CONTROL_NORMAL(0, kLookLR);
    mouse_dy_ += CONTROLS::GET_DISABLED_CONTROL_NORMAL(0, kLookUD);
    ++input_frames_;
    // what an interaction key did, in words -- not just "E was pressed"
    const int me = controlled_ped();
    if (me == 0) return;
    for (const int vk : {'W', 'A', 'S', 'D'}) if (keys_.count(vk)) acted_ = true;   // the person moves itself
    if (CONTROLS::IS_DISABLED_CONTROL_JUST_PRESSED(0, kEnter)) {
        const int veh = PED::GET_VEHICLE_PED_IS_TRYING_TO_ENTER(me);
        events_.push_back(veh != 0 ? "按 E：上车（抢车）" : (PED::IS_PED_IN_ANY_VEHICLE(me, FALSE) ? "按 E：下车" : "按 E：互动"));
    }
    if (CONTROLS::IS_DISABLED_CONTROL_JUST_PRESSED(0, kAttack)) events_.push_back("按鼠标左键：攻击/开火");
    if (CONTROLS::IS_DISABLED_CONTROL_JUST_PRESSED(0, kAim)) events_.push_back("按鼠标右键：瞄准");
    if (CONTROLS::IS_DISABLED_CONTROL_JUST_PRESSED(0, kMeleeLight) || CONTROLS::IS_DISABLED_CONTROL_JUST_PRESSED(0, kMeleeHeavy))
        events_.push_back("近身攻击");
    if (CONTROLS::IS_DISABLED_CONTROL_JUST_PRESSED(0, kJump)) events_.push_back("按空格：跳");
    if (CONTROLS::IS_DISABLED_CONTROL_JUST_PRESSED(0, kSprint)) events_.push_back("按 Shift：跑");
    if (!events_.empty()) acted_ = true;   // an interaction is the person's act too
}

// The free camera, flown on the game clock and recorded exactly on every tick
// boundary it passes: the tape's samples lie on the true path the 30 Hz grid
// cuts through, whatever the frames did (a sample read on the frame after a
// boundary is up to a frame late, and the jitter alone breaks the jerk limit
// many times over). It holds still until tick 0, so the tape's hold frames and
// its first tick agree.
void Take::fly(long long now_ms, double frame_dt) {
    if (free_.cam == 0) return;
    // look: the mouse turns the camera
    const double look_dt = std::max(frame_dt, 1e-3);
    double want_yaw = -CONTROLS::GET_DISABLED_CONTROL_NORMAL(0, kLookLR) * 8.0 / look_dt;
    double want_pitch = -CONTROLS::GET_DISABLED_CONTROL_NORMAL(0, kLookUD) * 8.0 / look_dt;
    const double turn = std::hypot(want_yaw, want_pitch);
    if (turn > kMaxRate) { want_yaw *= kMaxRate / turn; want_pitch *= kMaxRate / turn; }
    if ((free_.pitch.x > 75.0 && want_pitch > 0) || (free_.pitch.x < -75.0 && want_pitch < 0)) want_pitch = 0;   // eases to a stop
    // move: W/S forward, A/D sideways, E up, Q down -- at the speeds the program's
    // guard plans with (stage_robot.hpp FlyRoom)
    const double fwd = -CONTROLS::GET_DISABLED_CONTROL_NORMAL(0, kMoveUD);
    const double side = CONTROLS::GET_DISABLED_CONTROL_NORMAL(0, kMoveLR);
    const double up = ((GetAsyncKeyState('E') & 0x8000) ? 1.0 : 0.0) - ((GetAsyncKeyState('Q') & 0x8000) ? 1.0 : 0.0);
    const double h = free_.yaw.x * kPi / 180.0;
    const robot::FlyRoom room;
    // GTA heading h faces (-sin h, cos h); its right hand is (cos h, sin h)
    double tvx = (-std::sin(h) * fwd + std::cos(h) * side) * room.speed, tvy = (std::cos(h) * fwd + std::sin(h) * side) * room.speed;
    double tvz = up * room.climb;
    const double sp = std::sqrt(tvx * tvx + tvy * tvy + tvz * tvz);
    if (sp > kMaxSpeed) { tvx *= kMaxSpeed / sp; tvy *= kMaxSpeed / sp; tvz *= kMaxSpeed / sp; }
    const double floor_z = rt_.ground(free_.px.x, free_.py.x) + 0.3;
    if (free_.pz.x < floor_z + 0.5 && tvz < 0) tvz = 0;   // eases to a stop above the ground

    // each axis at 1/sqrt(3) (moving) or 1/sqrt(2) (turning) of the vector limits,
    // so the vector stays inside them
    const double a3 = kMaxAccel / std::sqrt(3.0), j3 = kMaxJerk / std::sqrt(3.0);
    const double a2 = kMaxAngAccel / std::sqrt(2.0), j2 = kMaxAngJerk / std::sqrt(2.0);
    const auto step = [&](double dt) {
        advance_axis(free_.px, tvx, dt, kMaxSpeed, a3, j3);
        advance_axis(free_.py, tvy, dt, kMaxSpeed, a3, j3);
        advance_axis(free_.pz, tvz, dt, kMaxSpeed, a3, j3);
        advance_axis(free_.yaw, want_yaw, dt, kMaxRate, a2, j2);
        advance_axis(free_.pitch, want_pitch, dt, kMaxRate, a2, j2);
    };
    const double end = static_cast<double>(now_ms);
    double t = std::max(free_.t_ms, static_cast<double>(perf0_));
    for (;;) {
        const double boundary = static_cast<double>(perf0_) + free_.next_tick * 1000.0 / kTickHz;
        if (boundary > end) break;
        if (boundary > t) { step((boundary - t) / 1000.0); t = boundary; }
        record_free(free_.next_tick++);
    }
    if (end > t) { step((end - t) / 1000.0); t = end; }
    free_.t_ms = std::max(free_.t_ms, t);
    // last resorts only: the easing above keeps these from acting
    if (free_.pz.x < floor_z) { free_.pz.x = floor_z; free_.pz.v = std::max(0.0, free_.pz.v); free_.pz.a = std::max(0.0, free_.pz.a); }
    free_.pitch.x = std::max(-85.0, std::min(85.0, free_.pitch.x));
    CAM::SET_CAM_COORD(free_.cam, static_cast<float>(free_.px.x), static_cast<float>(free_.py.x), static_cast<float>(free_.pz.x));
    CAM::SET_CAM_ROT(free_.cam, static_cast<float>(free_.pitch.x), 0.0f, static_cast<float>(norm_heading(free_.yaw.x)), 2);
}

// The program at the controls: what it sees, in stage coordinates, and the keys
// and mouse counts it sends for the next frame. It stops for good -- every key
// let go -- when the game loses the foreground or the mouse turns nothing.
void Take::drive(long long game_ms, double dt) {
    // D's person is the player from the start, so its mouse calibrates in the second
    // before tick 0 (the hold, cut from the delivery), no key down; AB's free camera
    // holds still until tick 0 (fly) and C's person is taken at the takeover.
    const long long early = cfg_.kind == TakeKind::D ? 1000 : 0;
    if (!robot_ || robot_stopped_ || control_ms_ < 0 || game_ms < control_ms_ - early) return;
    if (!game_in_front()) { stop_robot("the game window left the foreground: the program let go of every key and stopped"); return; }
    const StageFrame& fr = spec_.frame;
    const double fh = fr.heading_deg * kPi / 180.0;
    const auto local = [&](double wx, double wy, double& xs, double& ys) {
        const double dx = wx - fr.x, dy = wy - fr.y;
        xs = std::cos(fh) * dx + std::sin(fh) * dy;
        ys = -std::sin(fh) * dx + std::cos(fh) * dy;
    };
    const int me = controlled_ped();
    std::vector<robot::Person> people;
    double sx = 0, sy = 0;
    for (int i = 0; i < rt_.actors(); ++i) {
        const int p = rt_.ped(i);
        if (p == 0 || p == me) continue;
        const Vector3 at = ENTITY::GET_ENTITY_COORDS(p, TRUE);
        robot::Person q;
        q.id = rt_.actor_id(i);
        local(at.x, at.y, q.x, q.y);
        sx += q.x; sy += q.y;
        people.push_back(q);
    }
    const double t = (game_ms - control_ms_) / 1000.0;
    robot::Input in;
    if (flyer_) {
        robot::FlyView v;
        v.t = t; v.dt = dt;
        local(free_.px.x, free_.py.x, v.x, v.y);
        v.z = free_.pz.x - rt_.ground(free_.px.x, free_.py.x);
        v.yaw = robot::wrap(free_.yaw.x - fr.heading_deg);
        v.pitch = free_.pitch.x;
        if (!people.empty()) { v.fx = sx / people.size(); v.fy = sy / people.size(); }
        v.people = std::move(people);
        in = flyer_->step(v);
        if (flyer_->mouse().failed()) { stop_robot("the free camera did not turn when the program moved the mouse: its input does not reach the game"); return; }
    } else if ((walker_ || router_) && me != 0) {
        robot::WalkView v;
        v.t = t; v.dt = dt;
        const Vector3 at = ENTITY::GET_ENTITY_COORDS(me, TRUE);
        local(at.x, at.y, v.x, v.y);
        v.heading = robot::wrap(ENTITY::GET_ENTITY_HEADING(me) - fr.heading_deg);
        const Vector3 gr = CAM::_GET_GAMEPLAY_CAM_ROT(2);
        v.cam_yaw = robot::wrap(gr.z - fr.heading_deg);
        v.cam_pitch = gr.x;
        v.people = std::move(people);
        in = router_ ? router_->step(v) : walker_->step(v);
        body_to_ = std::isnan(in.body_to) ? std::nan("") : robot::wrap(in.body_to + fr.heading_deg);   // to world (hold_view)
        if ((router_ ? router_->mouse() : walker_->mouse()).failed()) {
            stop_robot("the game camera did not turn when the program moved the mouse: its input does not reach the game");
            return;
        }
    } else {
        return;
    }
    if (game_ms < control_ms_) in.keys = robot::Keys{};   // before tick 0: the mouse only
    press(in);
}

void Take::press(const robot::Input& in) {
    const bool want[6] = {in.keys.w, in.keys.a, in.keys.s, in.keys.d, in.keys.q, in.keys.e};
    bool* held[6] = {&held_.w, &held_.a, &held_.s, &held_.d, &held_.q, &held_.e};
    for (int i = 0; i < 6; ++i) {
        // a key held a third of a second must show as down: else the keys do not reach the game
        if (*held[i] && frames_ - held_since_[i] > 20 && (GetAsyncKeyState(kRobotKeys[i]) & 0x8000) == 0) {
            stop_robot(std::string("the key ") + static_cast<char>(kRobotKeys[i]) + " the program holds does not show as down");
            return;
        }
        if (want[i] == *held[i]) continue;
        send_key(kRobotKeys[i], want[i]);
        *held[i] = want[i];
        held_since_[i] = frames_;
    }
    if (in.mouse_dx != 0 || in.mouse_dy != 0) send_mouse(in.mouse_dx, in.mouse_dy);
}

void Take::release_keys() {
    bool* held[6] = {&held_.w, &held_.a, &held_.s, &held_.d, &held_.q, &held_.e};
    for (int i = 0; i < 6; ++i)
        if (*held[i]) { send_key(kRobotKeys[i], false); *held[i] = false; }
}

void Take::stop_robot(const std::string& why) {
    release_keys();
    robot_stopped_ = true;
    stats_.last_problem = "ROBOT: " + why;
    hooks_.log("  ROBOT: " + why);
}

void Take::record_free(int k) {
    CamSample& c = cams_[k];
    c.free = true;
    c.fpos[0] = free_.px.x; c.fpos[1] = free_.py.x; c.fpos[2] = free_.pz.x;
    c.frot[0] = free_.pitch.x; c.frot[1] = 0.0; c.frot[2] = norm_heading(free_.yaw.x);
    c.ffov = free_.fov;
    c.fvel[0] = free_.px.v; c.fvel[1] = free_.py.v; c.fvel[2] = free_.pz.v;
    c.fyaw_rate = free_.yaw.v; c.fpitch_rate = free_.pitch.v;
}

Take::CamSnap Take::read_cams(long long now_ms) const {
    CamSnap s;
    s.have = true;
    s.t = now_ms;
    const Vector3 gc = CAM::_GET_GAMEPLAY_CAM_COORDS();
    const Vector3 gr = CAM::_GET_GAMEPLAY_CAM_ROT(2);
    s.gpos[0] = gc.x; s.gpos[1] = gc.y; s.gpos[2] = gc.z;
    s.grot[0] = gr.x; s.grot[1] = gr.y; s.grot[2] = gr.z;
    s.gfov = CAM::GET_GAMEPLAY_CAM_FOV();
    const int me = controlled_ped();
    if (me != 0) {
        const Vector3 head = PED::GET_PED_BONE_COORDS(me, 0x796E, 0.f, 0.f, 0.f);
        const Vector3 at = ENTITY::GET_ENTITY_COORDS(me, TRUE);
        s.cid = controlled_id();
        s.head[0] = head.x; s.head[1] = head.y; s.head[2] = head.z;
        s.cpos[0] = at.x; s.cpos[1] = at.y; s.cpos[2] = at.z;
        s.ch = ENTITY::GET_ENTITY_HEADING(me);
    }
    return s;
}

// The game camera and the controlled person, read every frame and put on each
// tick boundary the frame passed by interpolating from the frame before.
void Take::capture_cams(long long now_ms) {
    const CamSnap s = read_cams(now_ms);
    const CamSnap& p = last_snap_;
    const int k_prev = p.have ? tick_of(p.t, perf0_) : -1;
    const int k_now = tick_of(now_ms, perf0_);
    for (int k = std::max(0, k_prev + 1); k <= k_now; ++k) {
        const double at = static_cast<double>(perf0_) + k * 1000.0 / kTickHz;
        const double w = p.have && now_ms > p.t ? std::max(0.0, std::min(1.0, (at - p.t) / static_cast<double>(now_ms - p.t))) : 1.0;
        const CamSnap& a = p.have ? p : s;
        CamSample& c = cams_[k];
        for (int i = 0; i < 3; ++i) {
            c.gpos[i] = lerp(a.gpos[i], s.gpos[i], w);
            c.grot[i] = lerp_deg(a.grot[i], s.grot[i], w);
        }
        c.gfov = lerp(a.gfov, s.gfov, w);
        c.cid = s.cid;
        if (!s.cid.empty()) {
            const bool same = a.cid == s.cid;   // across a takeover there is no frame before to start from
            for (int i = 0; i < 3; ++i) {
                c.head[i] = same ? lerp(a.head[i], s.head[i], w) : s.head[i];
                c.cpos[i] = same ? lerp(a.cpos[i], s.cpos[i], w) : s.cpos[i];
            }
            c.ch = same ? norm_heading(lerp_deg(a.ch, s.ch, w)) : s.ch;
        }
    }
    last_snap_ = s;
}

std::string Take::cams_json(int k) {
    auto it = cams_.find(k);
    if (it == cams_.end()) {   // never expected: what the cameras show now, rather than nothing
        ++stats_.cams_read_late;
        CamSample& c = cams_[k];
        const CamSnap s = read_cams(last_game_ms_);
        for (int i = 0; i < 3; ++i) { c.gpos[i] = s.gpos[i]; c.grot[i] = s.grot[i]; c.head[i] = s.head[i]; c.cpos[i] = s.cpos[i]; }
        c.gfov = s.gfov; c.cid = s.cid; c.ch = s.ch;
        if (free_.cam != 0) record_free(k);
        it = cams_.find(k);
    }
    const CamSample& c = it->second;
    std::string s = "\"cam_t_ms\":" + num(k * 1000.0 / kTickHz) + ",\"game_cam\":{\"pos\":" + vec3(c.gpos[0], c.gpos[1], c.gpos[2]) +
                    ",\"rot\":" + vec3(c.grot[0], c.grot[1], c.grot[2]) + ",\"fov\":" + num(c.gfov) + "}";
    if (c.free)
        s += ",\"free_cam\":{\"pos\":" + vec3(c.fpos[0], c.fpos[1], c.fpos[2]) + ",\"rot\":" + vec3(c.frot[0], c.frot[1], c.frot[2]) +
             ",\"fov\":" + num(c.ffov) + ",\"vel\":" + vec3(c.fvel[0], c.fvel[1], c.fvel[2]) + ",\"yaw_rate\":" + num(c.fyaw_rate) +
             ",\"pitch_rate\":" + num(c.fpitch_rate) + "}";
    if (!c.cid.empty())
        s += ",\"controlled\":{\"id\":" + json_str(c.cid) + ",\"head\":" + vec3(c.head[0], c.head[1], c.head[2]) + ",\"pos\":" +
             vec3(c.cpos[0], c.cpos[1], c.cpos[2]) + ",\"h\":" + num(c.ch) + "}";
    else
        s += ",\"controlled\":null";
    cams_.erase(cams_.begin(), cams_.upper_bound(k));   // this tick's and any skipped before it
    return s;
}

void Take::sample_pending() {
    Pending& p = pending_;
    const int n = rt_.actors();
    p.actors.assign(static_cast<size_t>(n), EntitySample{});
    p.beats.assign(static_cast<size_t>(n), -1);
    p.released.assign(static_cast<size_t>(n), false);
    for (int i = 0; i < n; ++i) {
        rt_.sample_actor(i, p.actors[static_cast<size_t>(i)]);
        p.released[static_cast<size_t>(i)] = rt_.released(i);
        p.beats[static_cast<size_t>(i)] = tick_op(plan_, cal_, i, p.tick - rt_.delay(i)).beat;
    }
    Runtime::sample_ped(PLAYER::PLAYER_PED_ID(), p.player);
    p.map.assign(static_cast<size_t>(rt_.tracks()), MapSample{});
    for (int i = 0; i < rt_.tracks(); ++i) rt_.sample_track(i, p.map[static_cast<size_t>(i)]);
    p.wind_speed = GAMEPLAY::GET_WIND_SPEED();
    const Vector3 wd = GAMEPLAY::GET_WIND_DIRECTION();
    p.wind_dir[0] = wd.x; p.wind_dir[1] = wd.y; p.wind_dir[2] = wd.z;
    p.props.assign(static_cast<size_t>(rt_.props()), EntitySample{});
    for (int i = 0; i < rt_.props(); ++i) rt_.sample_prop(i, p.props[static_cast<size_t>(i)]);
    // the cameras on the tick's own boundary: the game camera as rendered, the
    // free camera as flown, the controlled person's head for the first-person lane
    p.cams = cams_json(p.tick);
    p.sampled = true;
}

// Writes the tick that just ended. Its props are read now, at its end: they
// trail the skeleton by a frame, and a pose that was up for one frame only (a
// capped step followed by a short one) is marked, not trusted.
void Take::finish_tick(long long frame_now) {
    Pending& p = pending_;
    if (p.tick < 0) return;
    if (!p.sampled) sample_pending();
    for (int i = 0; i < rt_.props() && i < static_cast<int>(p.props.size()); ++i) rt_.sample_prop(i, p.props[static_cast<size_t>(i)]);
    const long long held = std::min<long long>(255, frame_now - posed_frame_);
    std::ostringstream w;
    w << "{\"schema\":\"dm-gta-world-telemetry/1\",\"tick\":" << p.tick << ",\"game_ms\":" << (p.game_ms - perf0_) << ",\"held\":" << held
      << ",\"skipped_before\":" << p.skipped_before << ",\"actors\":[";
    for (size_t i = 0; i < p.actors.size(); ++i) {
        w << (i ? "," : "") << "{\"id\":" << json_str(rt_.actor_id(static_cast<int>(i))) << ",\"released\":" << (p.released[i] ? 1 : 0)
          << ",\"beat\":" << p.beats[i] << ",\"state\":" << entity_json(p.actors[i], true) << "}";
    }
    w << "],\"props\":[";
    for (size_t i = 0; i < p.props.size(); ++i)
        w << (i ? "," : "") << "{\"id\":" << json_str(rt_.prop_id(static_cast<int>(i))) << ",\"state\":" << entity_json(p.props[i], false) << "}";
    w << "],\"map\":[";
    for (size_t i = 0; i < p.map.size(); ++i) {
        const MapSample& m = p.map[i];
        w << (i ? "," : "") << "{\"id\":" << json_str(rt_.track_id(static_cast<int>(i))) << ",\"state\":";
        if (m.present) w << "{\"pos\":" << vec3(m.pos) << ",\"quat\":[" << num(m.quat[0]) << "," << num(m.quat[1]) << "," << num(m.quat[2]) << ","
                         << num(m.quat[3]) << "]}";
        else w << "null";
        w << "}";
    }
    w << "],\"env\":{\"wind_speed\":" << num(p.wind_speed) << ",\"wind_dir\":" << vec3(p.wind_dir) << ",\"cleared\":" << stats_.cleared << "}";
    w << ",\"player\":" << entity_json(p.player, true) << "}\n";
    world_ << w.str();
    if (!world_) stats_.logs_ok = false;
    std::ostringstream in;
    in << "{\"schema\":\"dm-gta-input-camera-log/1\",\"tick\":" << p.tick << ",\"game_ms\":" << (p.game_ms - perf0_)
       << ",\"wall\":" << json_str(p.wall) << ",\"src\":" << (!robot_ ? "\"person\"" : robot_stopped_ ? "\"stopped\"" : "\"program\"") << ",\"frames\":" << input_frames_
       << ",\"keyCode\":[";
    bool first = true;
    for (int k : keys_) { in << (first ? "" : ",") << k; first = false; }
    in << "],\"mouse_dx\":" << num(mouse_dx_) << ",\"mouse_dy\":" << num(mouse_dy_) << ",\"events\":[";
    for (size_t i = 0; i < events_.size(); ++i) in << (i ? "," : "") << json_str(events_[i]);
    in << "]," << p.cams << "}\n";
    input_ << in.str();
    if (!input_) stats_.logs_ok = false;
    ++stats_.ticks_sampled;
    keys_.clear(); mouse_dx_ = mouse_dy_ = 0; input_frames_ = 0; events_.clear();
    p = Pending{};
}

// What a take does on every frame, a tick starting on it or not -- the frames
// it waits for new scenes to take hold included: the controls stay blocked, the
// free camera flies, the cameras and the input are read.
void Take::each_frame(long long game_ms) {
    ++frames_;
    const double dt = std::max(0.0, (game_ms - last_game_ms_) / 1000.0);
    last_game_ms_ = std::max(last_game_ms_, game_ms);
    // No control is blocked: the hidden body is frozen (AB) or rides on its actor
    // (C before the takeover), so the keys move nothing but the free camera. And
    // blocking is what lost take AB (2026-09-23): DISABLE_ALL_CONTROL_ACTIONS
    // stops the recorder on the very next frame (recorder self-test).
    // GTA's idle camera takes the game camera to the played person's head after
    // ~30 s without input and hands it back on the next key; the chase lane is
    // that camera, so take D (2026-09-23) carried 13 s of first person in it.
    // Reset the idle timer every frame (INVALIDATE_IDLE_CAM) - no control blocked.
    CAM::_0xF4F2C0D4EE209E20();
    hold_view();
    timing_ = FrameTiming{};
    double t0 = perf_now_ms();
    fly(game_ms, dt);
    double t1 = perf_now_ms(); timing_.fly = t1 - t0; t0 = t1;
    // What a script reads is what the last frame made -- the game updates bodies and
    // its camera after the scripts -- so it is stamped with that frame's game time.
    // Stamped with this frame's, every sample was a frame's (varying) length late and
    // the chase lane shook (beach-demo-002 C: jerk p95 1570 m/s^3 on the tape).
    const long long shown_ms = prev_frame_ms_;
    prev_frame_ms_ = game_ms;
    if (shown_ms >= 0) capture_cams(shown_ms);
    t1 = perf_now_ms(); timing_.cams = t1 - t0; t0 = t1;
    // The tick posed last frame is sampled on the first frame after it -- after the
    // capture above, which puts the cameras on its boundary. Sampled before it, no
    // tick had its cameras yet: take C of beach-demo-003 read all 3599 off their
    // boundary, and AB (the free camera made each tick's entry) logged the game
    // camera as zeros (2026-09-28).
    if (pending_.tick >= 0 && !pending_.sampled) sample_pending();
    t1 = perf_now_ms(); timing_.sample = t1 - t0; t0 = t1;
    accumulate_input();
    drive(game_ms, dt);   // the program's keys for the next frame, when a program plays
    t1 = perf_now_ms(); timing_.drive = t1 - t0; t0 = t1;
    for (int i = 0; i < rt_.actors(); ++i) {
        const int p = rt_.ped(i);
        if (p == 0 || rt_.released(i)) continue;
        PED::SET_BLOCKING_OF_NON_TEMPORARY_EVENTS(p, TRUE);
        PED::SET_PED_CAN_RAGDOLL(p, FALSE);
    }
}

std::string Take::frame(long long game_ms) {
    if (ended_) return "";
    const double t_frame = perf_now_ms();
    each_frame(game_ms);

    const int k = tick_of(game_ms, perf0_);
    if (k <= last_tick_ || k < 0 || k >= plan_.ticks) { timing_.total = perf_now_ms() - t_frame; return ""; }
    timing_.tick = k;
    const int skipped = last_tick_ >= 0 ? k - last_tick_ - 1 : k;
    stats_.ticks_skipped += skipped;
    double t0 = perf_now_ms();
    finish_tick(frames_);
    double t1 = perf_now_ms(); timing_.write = t1 - t0; t0 = t1;

    if (cfg_.kind == TakeKind::C && !taken_ && stats_.takeover_tick < 0 && k >= cfg_.takeover_tick) take_over(k);
    evaluate_rules(k);
    for (size_t i = 0; i < watching_.size();) {
        if (k < watching_[i].until) { ++i; continue; }
        carry_on(watching_[i], k);
        watching_.erase(watching_.begin() + static_cast<std::ptrdiff_t>(i));
    }
    for (Routine& r : routines_) advance_routine(r, k);
    routines_.erase(std::remove_if(routines_.begin(), routines_.end(), [](const Routine& r) { return r.finished; }),
                    routines_.end());
    rt_.retire_due(k);
    t1 = perf_now_ms(); timing_.rules = t1 - t0; t0 = t1;
    // New scenes are checked kCheckAfterTicks on, once their tasks have held. (They
    // used to be checked two extra frames in, inside their own tick; under the
    // recorder's 30 fps cap those frames cost two ticks -- the cast stalled, then
    // jumped -- and made 32 frames of a second.)
    const std::string bad = rt_.verify(plan_, k - kCheckAfterTicks);
    if (!bad.empty()) { stats_.last_problem = bad; hooks_.log("  SCENE at tick " + std::to_string(k) + ": " + bad); }
    t1 = perf_now_ms(); timing_.verify = t1 - t0; t0 = t1;
    rt_.enter(plan_, cal_, k);
    rt_.pose(plan_, cal_, k);
    t1 = perf_now_ms(); timing_.pose = t1 - t0; t0 = t1;
    timing_.total = t1 - t_frame;
    pending_ = Pending{};
    pending_.tick = k;
    pending_.game_ms = game_ms;
    pending_.wall = local_wall_now();
    pending_.skipped_before = skipped;
    posed_frame_ = frames_;
    last_tick_ = k;
    return "";
}

void Take::end() {
    if (ended_) return;
    ended_ = true;
    release_keys();   // before anything else: no key stays down after the take
    g_key_pump.drain_and_stop();   // the releases sent, and no thread of ours left behind
    if (router_) {   // how the route went: what to check the recording against
        char line[200];
        std::snprintf(line, sizeof line, "  ROBOT: route spots reached %zu, stopped over 0.5 m off %d, held back for somebody %d, widest start turn %.0f deg",
                      router_->arrived(), router_->missed(), router_->held_back(), router_->start_turn());
        hooks_.log(line);
    }
    if (pending_.tick >= 0) finish_tick(frames_ + 1);
    // ticks the take never reached (a capped step jumped past the last ones, or
    // the recorder stopped) are skipped ticks like any other: the replay skips them too
    if (last_tick_ < plan_.ticks - 1) stats_.ticks_skipped += plan_.ticks - 1 - last_tick_;
    if (free_.cam != 0) {
        CAM::RENDER_SCRIPT_CAMS(FALSE, FALSE, 0, TRUE, FALSE);
        CAM::SET_CAM_ACTIVE(free_.cam, FALSE);
        CAM::DESTROY_CAM(free_.cam, FALSE);
        free_.cam = 0;
    }
    // the player gets its own story character back before anything is deleted
    if (own_body_ != 0 && ENTITY::DOES_ENTITY_EXIST(own_body_)) {
        ENTITY::DETACH_ENTITY(own_body_, TRUE, FALSE);
        if (PLAYER::PLAYER_PED_ID() != own_body_) PLAYER::CHANGE_PLAYER_PED(PLAYER::PLAYER_ID(), own_body_, FALSE, FALSE);
        ENTITY::SET_ENTITY_VISIBLE(own_body_, TRUE, FALSE);
        ENTITY::SET_ENTITY_COLLISION(own_body_, TRUE, TRUE);
        ENTITY::FREEZE_ENTITY_POSITION(own_body_, FALSE);
        ENTITY::SET_ENTITY_INVINCIBLE(own_body_, FALSE);
    } else if (own_body_ != 0) {
        stats_.last_problem = "the player's story character was gone at the end of the take";
        hooks_.log("  CLEANUP: " + stats_.last_problem + " -- the player keeps the body it has");
    }
    if (saved_view_mode_ >= 0) CAM::SET_FOLLOW_PED_CAM_VIEW_MODE(saved_view_mode_);   // the view the player had before
    if (seventh_ != 0 && ENTITY::DOES_ENTITY_EXIST(seventh_)) {
        if (PLAYER::PLAYER_PED_ID() == seventh_) {
            hooks_.log("  CLEANUP: the seventh person is the player's body right now -- left in place");
        } else {
            ENTITY::SET_ENTITY_AS_MISSION_ENTITY(seventh_, TRUE, TRUE);
            Ped s = seventh_;
            PED::DELETE_PED(&s);
        }
    }
    seventh_ = 0;
    write_scan("map-scan-end.jsonl");
    if (!spec_.clears.empty()) {
        stats_.cleared_left = rt_.cleared_left();
        hooks_.log("  CLEAR: " + std::to_string(stats_.cleared) + " of the map's objects removed before the recorder, " +
                   std::to_string(stats_.cleared_left) + " back at the end");
    }
    rt_.despawn();
    cams_.clear();
    if (!world_path_.empty()) {   // opened by begin(): whatever did not reach the disk fails the take
        world_.close(); input_.close(); rules_.close();
        if (world_.fail() || input_.fail() || rules_.fail()) stats_.logs_ok = false;
    }
}

}  // namespace diogenes::gta::stage
