#include "story_projection.hpp"

#include "controller_core.hpp"
#include "fx_capture_observer.hpp"
#include "hash_utils.hpp"

#include <main.h>
#include <natives.h>
#include <windows.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <cmath>
#include <filesystem>
#include <fstream>
#include <iomanip>
#include <sstream>
#include <stdexcept>

namespace diogenes::gta {
namespace {

using Clock = std::chrono::steady_clock;
using Vec3 = std::array<double, 3>;
using Quat = std::array<double, 4>;
constexpr double kPi = 3.14159265358979323846;
constexpr double kPositionTolerance = 0.001;
constexpr double kRotationToleranceDeg = 0.01;
constexpr double kFovToleranceDeg = 0.001;
constexpr double kProjectionTolerance = 0.000001;
std::atomic_flag g_story_sample_active = ATOMIC_FLAG_INIT;

struct ActiveLease {
    ~ActiveLease() { g_story_sample_active.clear(std::memory_order_release); }
};

std::int64_t qpc_now() {
    LARGE_INTEGER value{};
    QueryPerformanceCounter(&value);
    return value.QuadPart;
}

template <std::size_t N>
bool finite(const std::array<double, N>& values) {
    return std::all_of(values.begin(), values.end(), [](double v) { return std::isfinite(v); });
}

Quat multiply(const Quat& a, const Quat& b) {
    return {a[3]*b[0]+a[0]*b[3]+a[1]*b[2]-a[2]*b[1],
            a[3]*b[1]-a[0]*b[2]+a[1]*b[3]+a[2]*b[0],
            a[3]*b[2]+a[0]*b[1]-a[1]*b[0]+a[2]*b[3],
            a[3]*b[3]-a[0]*b[0]-a[1]*b[1]-a[2]*b[2]};
}

Quat order2_quaternion(const Vec3& euler) {
    // Same GTA order-2 contract as gta_capture/tape.py: qz * qx * qy.
    const double x=euler[0]*kPi/360, y=euler[1]*kPi/360, z=euler[2]*kPi/360;
    return multiply({0,0,std::sin(z),std::cos(z)},
        multiply({std::sin(x),0,0,std::cos(x)}, {0,std::sin(y),0,std::cos(y)}));
}

double position_error(const Vec3& a, const Vec3& b) {
    double d=0;
    for (std::size_t i=0;i<3;++i) d+=(a[i]-b[i])*(a[i]-b[i]);
    return std::sqrt(d);
}

double rotation_error(const Quat& a, const Quat& b) {
    double minus=0, plus=0;
    for (std::size_t i=0;i<4;++i) {
        minus+=(a[i]-b[i])*(a[i]-b[i]);
        plus+=(a[i]+b[i])*(a[i]+b[i]);
    }
    return 4*std::asin((std::min)(1.0,std::sqrt((std::min)(minus,plus))/2))*180/kPi;
}

struct Observation {
    Vec3 position{};
    Vec3 euler{};
    Quat quaternion{};
    double fov{};
    std::array<double,2> screen{};
    bool visible{};
    int frame{};
    std::int64_t qpc{};
};

Observation observe(const Vec3& point) {
    Observation value;
    value.qpc=qpc_now();
    value.frame=GAMEPLAY::GET_FRAME_COUNT();
    // These old SDK aliases identify GET_FINAL_RENDERED_CAM_COORD/ROT.
    const auto position=CAM::_GET_GAMEPLAY_CAM_COORDS();
    const auto euler=CAM::_GET_GAMEPLAY_CAM_ROT(2);
    value.position={position.x,position.y,position.z};
    value.euler={euler.x,euler.y,euler.z};
    value.quaternion=order2_quaternion(value.euler);
    // The SDK's named 0x80EC... wrapper returns Any; request float explicitly.
    value.fov=invoke<float>(0x80EC114669DAEFF4);
    float sx=0, sy=0;
    value.visible=GRAPHICS::_WORLD3D_TO_SCREEN2D(
        static_cast<float>(point[0]),static_cast<float>(point[1]),
        static_cast<float>(point[2]),&sx,&sy) != FALSE;
    value.screen={sx,sy};
    if (value.frame != GAMEPLAY::GET_FRAME_COUNT() || !finite(value.position) ||
        !finite(value.euler) || !finite(value.screen) || !std::isfinite(value.fov)) {
        throw std::runtime_error("native camera/projection observation is unstable or non-finite");
    }
    return value;
}

bool stable(const Observation& a, const Observation& b) {
    return position_error(a.position,b.position)<=kPositionTolerance &&
        rotation_error(a.quaternion,b.quaternion)<=kRotationToleranceDeg &&
        std::abs(a.fov-b.fov)<=kFovToleranceDeg && a.visible==b.visible &&
        std::abs(a.screen[0]-b.screen[0])<=kProjectionTolerance &&
        std::abs(a.screen[1]-b.screen[1])<=kProjectionTolerance;
}

bool matches_request(const Observation& value, const StoryProjectionRequest& request) {
    return position_error(value.position,request.camera_position)<=kPositionTolerance &&
        rotation_error(value.quaternion,order2_quaternion(request.camera_euler_deg))<=kRotationToleranceDeg &&
        std::abs(value.fov-request.fov_deg)<=kFovToleranceDeg;
}

class CameraLease {
public:
    ~CameraLease() { restore(); }
    void create(const StoryProjectionRequest& request) {
        const Cam prior=CAM::GET_RENDERING_CAM();
        if (CAM::DOES_CAM_EXIST(prior)) {
            throw std::runtime_error("Story sampling refuses an existing rendering scripted camera");
        }
        if (!CAM::IS_GAMEPLAY_CAM_RENDERING()) {
            throw std::runtime_error("Story sampling requires the ordinary gameplay camera before takeover");
        }
        char name[]="DEFAULT_SCRIPTED_CAMERA";
        camera_=CAM::CREATE_CAM(name,FALSE);
        if (!CAM::DOES_CAM_EXIST(camera_)) {
            throw std::runtime_error("cannot create the private Story calibration camera");
        }
        hold(request);
        CAM::SET_CAM_MOTION_BLUR_STRENGTH(camera_,0.0f);
        CAM::SET_CAM_DOF_STRENGTH(camera_,0.0f);
        CAM::SET_CAM_ACTIVE(camera_,TRUE);
        CAM::RENDER_SCRIPT_CAMS(TRUE,FALSE,0,TRUE,FALSE);
        render_enabled_=true;
    }
    bool owns_rendering_camera() const {
        return camera_ != 0 && CAM::DOES_CAM_EXIST(camera_) &&
               CAM::GET_RENDERING_CAM()==camera_;
    }
    void hold(const StoryProjectionRequest& request) const {
        CAM::SET_CAM_COORD(camera_,static_cast<float>(request.camera_position[0]),
            static_cast<float>(request.camera_position[1]),static_cast<float>(request.camera_position[2]));
        CAM::SET_CAM_ROT(camera_,static_cast<float>(request.camera_euler_deg[0]),
            static_cast<float>(request.camera_euler_deg[1]),static_cast<float>(request.camera_euler_deg[2]),2);
        CAM::SET_CAM_FOV(camera_,static_cast<float>(request.fov_deg));
    }
    bool restore() noexcept {
        if (camera_==0) return true;
        try {
            const bool own_render=CAM::GET_RENDERING_CAM()==camera_;
            // Never switch off a camera installed by somebody else meanwhile.
            if (CAM::DOES_CAM_EXIST(camera_)) CAM::SET_CAM_ACTIVE(camera_,FALSE);
            if (render_enabled_ && own_render) CAM::RENDER_SCRIPT_CAMS(FALSE,FALSE,0,TRUE,FALSE);
            if (CAM::DOES_CAM_EXIST(camera_)) CAM::DESTROY_CAM(camera_,TRUE);
            if (CAM::DOES_CAM_EXIST(camera_)) return false;
        } catch (...) {
            return false;
        }
        camera_=0;
        render_enabled_=false;
        return true;
    }
private:
    Cam camera_{};
    bool render_enabled_{};
};

void draw_cross(const StoryProjectionRequest& request) {
    if (!request.draw_marker) return;
    // Geometry is positioned in world space. No projected screen coordinates
    // are used for drawing. The two exact 3D line segments intersect at point.
    const auto q=order2_quaternion(request.camera_euler_deg);
    const double x=q[0], y=q[1], z=q[2], w=q[3];
    const std::array<Vec3,2> directions{{
        {1-2*(y*y+z*z),2*(x*y+w*z),2*(x*z-w*y)},
        {2*(x*z+w*y),2*(y*z-w*x),1-2*(x*x+y*y)}}};
    for (const auto& axis:directions) {
        Vec3 a{},b{};
        for (std::size_t i=0;i<3;++i) {
            a[i]=request.point_world[i]-0.04*axis[i];
            b[i]=request.point_world[i]+0.04*axis[i];
        }
        GRAPHICS::DRAW_LINE(static_cast<float>(a[0]),static_cast<float>(a[1]),static_cast<float>(a[2]),
            static_cast<float>(b[0]),static_cast<float>(b[1]),static_cast<float>(b[2]),255,0,255,255);
    }
}

template <std::size_t N>
void write_array(std::ostringstream& out, const std::array<double,N>& values) {
    out << '[';
    for (std::size_t i=0;i<N;++i) { if (i) out << ','; out << values[i]; }
    out << ']';
}

void write_observation(std::ostringstream& out, const Observation& value) {
    out << "{\"position\":"; write_array(out,value.position);
    out << ",\"euler_deg\":"; write_array(out,value.euler);
    out << ",\"quaternion\":"; write_array(out,value.quaternion);
    out << ",\"fov_deg\":" << value.fov << ",\"screen_normalized\":";
    write_array(out,value.screen);
    out << ",\"projection_visible\":" << (value.visible?"true":"false")
        << ",\"game_frame\":" << value.frame << ",\"qpc\":" << value.qpc << '}';
}

std::array<std::uint32_t,2> png_dimensions(const std::string& path) {
    std::ifstream input(std::filesystem::u8path(path),std::ios::binary);
    std::array<unsigned char,24> bytes{};
    input.read(reinterpret_cast<char*>(bytes.data()),static_cast<std::streamsize>(bytes.size()));
    const std::array<unsigned char,8> magic{{137,80,78,71,13,10,26,10}};
    if (!input || !std::equal(magic.begin(),magic.end(),bytes.begin()) ||
        bytes[12]!='I' || bytes[13]!='H' || bytes[14]!='D' || bytes[15]!='R') {
        throw std::runtime_error("Story capture output is not a complete PNG header");
    }
    const auto read32=[&](std::size_t offset) {
        return (std::uint32_t(bytes[offset])<<24)|(std::uint32_t(bytes[offset+1])<<16)|
               (std::uint32_t(bytes[offset+2])<<8)|std::uint32_t(bytes[offset+3]);
    };
    return {read32(16),read32(20)};
}

}  // namespace

AdapterResult capture_story_projection(const StoryProjectionRequest& requested) {
    if (g_story_sample_active.test_and_set(std::memory_order_acquire)) {
        return AdapterResult::failure("another Story calibration sample is active");
    }
    ActiveLease active_lease;
    // The native ABI and DRAW_LINE take float. Preserve the actual values
    // submitted to the engine rather than labeling a nearby double as measured.
    StoryProjectionRequest request=requested;
    for (std::size_t axis=0;axis<3;++axis) {
        request.camera_position[axis]=static_cast<float>(requested.camera_position[axis]);
        request.camera_euler_deg[axis]=static_cast<float>(requested.camera_euler_deg[axis]);
        request.point_world[axis]=static_cast<float>(requested.point_world[axis]);
    }
    request.fov_deg=static_cast<float>(requested.fov_deg);
    const auto start=Clock::now();
    const auto capture_deadline=start+std::chrono::seconds(8);
    const auto deadline=start+std::chrono::seconds(10);
    FxCaptureObserver transport;
    EditorIdentityObserver editor;
    CameraLease camera;
    std::uint32_t request_id=0, transport_generation=0;
    std::uint32_t waits=0;
    bool released=false;
    std::string error;
    const auto check_context=[&]() {
        EditorIdentitySnapshot identity;
        if (NETWORK::NETWORK_IS_SESSION_ACTIVE() || NETWORK::NETWORK_IS_IN_SESSION() ||
            NETWORK::NETWORK_IS_GAME_IN_PROGRESS() || UI::IS_PAUSE_MENU_ACTIVE() ||
            !editor.read(identity,error) || identity.editor_active || identity.render_active) {
            throw std::runtime_error("Story script context changed or Editor identity is unavailable: "+error);
        }
    };
    const auto step=[&]() {
        check_context();
        camera.hold(request);
        UI::HIDE_HUD_AND_RADAR_THIS_FRAME();
        draw_cross(request);
        WAIT(0);
        ++waits;
        check_context();
        if (!camera.owns_rendering_camera()) {
            throw std::runtime_error("private Story camera lost rendering ownership");
        }
    };
    try {
        if (!finite(request.camera_position) || !finite(request.camera_euler_deg) ||
            !finite(request.point_world) || !std::isfinite(request.fov_deg) ||
            request.fov_deg<=1 || request.fov_deg>=179 ||
            !std::isfinite(request.nominal_focal_mm) || request.nominal_focal_mm<=0) {
            throw std::runtime_error("Story projection request contains invalid camera/point/focal values");
        }
        if (!transport.open(error) || !editor.open(error)) throw std::runtime_error(error);
        check_context();
        FxCaptureSnapshot initial;
        if (!transport.read(initial,error) || initial.request_id!=initial.ack_id) {
            throw std::runtime_error("Story capture transport is not idle: "+error);
        }
        camera.create(request);
        Observation before{},previous{};
        unsigned stable_ticks=0, settled_ticks=0;
        while (Clock::now()<capture_deadline && (settled_ticks<12 || stable_ticks<4)) {
            step();
            before=observe(request.point_world);
            if (settled_ticks && before.frame==previous.frame) continue;
            ++settled_ticks;
            if (matches_request(before,request) && settled_ticks>1 && stable(before,previous)) ++stable_ticks;
            else stable_ticks=0;
            previous=before;
        }
        if (settled_ticks<12 || stable_ticks<4 || !matches_request(before,request)) {
            throw std::runtime_error("Story final-rendered camera did not settle within the capture deadline");
        }
        if (!before.visible || before.screen[0]<0 || before.screen[0]>1 || before.screen[1]<0 || before.screen[1]>1) {
            throw std::runtime_error("native projection reports point outside the frame");
        }
        FxCaptureSnapshot heartbeat;
        if (!transport.read(heartbeat,error) || heartbeat.addon_heartbeat==initial.addon_heartbeat) {
            throw std::runtime_error("Igcs capture heartbeat did not advance during Story settling");
        }
        // Redraw for the request's render frame after the final native sample.
        UI::HIDE_HUD_AND_RADAR_THIS_FRAME();
        draw_cross(request);
        if (!transport.publish_story_request(request.output_path,request_id,transport_generation,error)) {
            throw std::runtime_error(error);
        }
        const auto published_qpc=qpc_now();
        FxCaptureSnapshot captured;
        bool ready=false;
        while (Clock::now()<capture_deadline) {
            step();
            const auto current=observe(request.point_world);
            if (!stable(before,current) || !matches_request(current,request)) {
                throw std::runtime_error("Story camera or native projection changed while pixels were pending");
            }
            if (!transport.read(captured,error)) throw std::runtime_error(error);
            if (captured.request_id!=request_id || captured.request_camera_generation!=transport_generation ||
                captured.out_path!=request.output_path) {
                throw std::runtime_error("Story transport request ownership changed");
            }
            if (captured.capture_ready_id==request_id) { ready=true; break; }
        }
        if (!ready) throw std::runtime_error("Story PNG pixel-ready timed out");
        const auto ready_qpc=qpc_now();
        if (captured.status!=0 || captured.capture_ready_camera_generation!=transport_generation ||
            captured.ack_id==request_id || captured.width==0 || captured.height==0) {
            throw std::runtime_error("Story pixel-ready status/transport epoch/dimensions are invalid");
        }
        // Independently bracket the completed readback with a subsequent game
        // tick, while retaining the same static camera and world-space marker.
        step();
        const auto after=observe(request.point_world);
        if (!stable(before,after) || !matches_request(after,request)) {
            throw std::runtime_error("Story post-ready native camera/projection differs from pre-capture");
        }
        if (!transport.release(request_id,error)) throw std::runtime_error(error);
        released=true;
        bool acked=false;
        while (Clock::now()<capture_deadline) {
            step();
            FxCaptureSnapshot ack;
            if (!transport.read(ack,error)) throw std::runtime_error(error);
            if (ack.request_id!=request_id) throw std::runtime_error("Story request replaced before ACK");
            if (ack.ack_id==request_id) { acked=true; break; }
        }
        if (!acked) throw std::runtime_error("Story observer release did not receive ACK");
        const auto ack_qpc=qpc_now();
        const auto dimensions=png_dimensions(request.output_path);
        const auto bytes=std::filesystem::file_size(std::filesystem::u8path(request.output_path));
        if (dimensions[0]!=captured.width || dimensions[1]!=captured.height || bytes<24 || bytes>128*1024*1024) {
            throw std::runtime_error("Story PNG dimensions/size do not match the completed readback");
        }
        std::string digest;
        if (!sha256_file(request.output_path,digest,error)) throw std::runtime_error(error);
        if (!camera.restore()) throw std::runtime_error("private Story camera could not be destroyed");
        LARGE_INTEGER frequency{};
        QueryPerformanceFrequency(&frequency);
        std::ostringstream out;
        out << std::setprecision(17)
            << "{\"schema\":\"dm-gta-story-calibration-frame/1\",\"mock\":false,"
               "\"approval_status\":\"draft\",\"editor_equivalence_verified\":false,"
               "\"delivery_eligible\":false,\"output_path\":\"" << json_escape(request.output_path)
            << "\",\"output_sha256\":\"" << digest << "\",\"output_bytes\":" << bytes
            << ",\"width\":" << dimensions[0] << ",\"height\":" << dimensions[1]
            << ",\"nominal_focal_mm\":" << request.nominal_focal_mm
            << ",\"point_world\":"; write_array(out,request.point_world);
        out << ",\"requested_point_world\":"; write_array(out,requested.point_world);
        out << ",\"screen_normalized\":"; write_array(out,before.screen);
        out << ",\"projection_visible\":true,\"projection_method\":\"GET_SCREEN_COORD_FROM_WORLD_COORD\","
               "\"projection_native_hash\":\"0x34E82F05DF2974F5\","
               "\"camera_before\":"; write_observation(out,before);
        out << ",\"camera_after\":"; write_observation(out,after);
        out << ",\"camera_coordinate_native_hash\":\"0xA200EB1EE790F448\","
               "\"camera_rotation_native_hash\":\"0x5B4E4C817FCC2DFB\","
               "\"camera_fov_native_hash\":\"0x80EC114669DAEFF4\","
               "\"quaternion_source\":\"qz*qx*qy from native final-rendered Euler order 2\","
               "\"marker_mode\":\"" << (request.draw_marker?"world_cross":"none")
            << "\",\"marker_line_length_world_units\":0.08,\"marker_rgb\":[255,0,255],"
               "\"scripted_camera_motion_blur_strength\":0,\"scripted_camera_dof_strength\":0,"
               "\"render_marker_crosscheck\":\"pending independent PNG pixel detection\","
               "\"transport\":{\"protocol\":18,\"request_id\":" << request_id
            << ",\"generation_echo\":" << transport_generation
            << ",\"pixel_ready_id\":" << captured.capture_ready_id
            << ",\"pixel_ready_generation_echo\":" << captured.capture_ready_camera_generation
            << ",\"observer_release_id\":" << request_id << ",\"ack_id\":" << request_id
            << ",\"editor_writer_fence\":false,\"qpc_frequency\":" << frequency.QuadPart
            << ",\"published_qpc\":" << published_qpc << ",\"ready_observed_qpc\":" << ready_qpc
            << ",\"ack_observed_qpc\":" << ack_qpc << "},\"settled_game_ticks\":" << settled_ticks
            << ",\"consecutive_stable_ticks\":" << stable_ticks << ",\"cooperative_waits\":" << waits
            << ",\"camera_stability_limits\":{\"position_world_units\":" << kPositionTolerance
            << ",\"rotation_deg\":" << kRotationToleranceDeg << ",\"fov_deg\":" << kFovToleranceDeg
            << ",\"screen_normalized\":" << kProjectionTolerance << "},"
               "\"capture_semantics\":\"static-camera native observations bracket an Igcs PNG; not an Editor generation fence\","
               "\"camera_restoration\":{\"private_camera_destroyed\":true,"
               "\"gameplay_restore_requested\":true,\"restored_view_pixel_verified\":false}}";
        return AdapterResult::success(out.str());
    } catch (const std::exception& exc) {
        error=exc.what();
    } catch (...) {
        error="unexpected error in Story projection capture";
    }
    std::string drain=request_id==0?"not_published":"pending";
    // Release only this request, and only after its add-on pixel-ready fact.
    // Never reset/clear a timed-out shared block or acknowledge another writer.
    while (request_id!=0 && Clock::now()<deadline) {
        FxCaptureSnapshot state;
        std::string detail;
        if (!transport.read(state,detail)) { drain="unreadable: "+detail; break; }
        if (state.request_id!=request_id) { drain="ownership_changed"; break; }
        if (state.ack_id==request_id) { drain="acknowledged"; break; }
        if (state.capture_ready_id==request_id && !released) {
            if (!transport.release(request_id,detail)) { drain="release_failed: "+detail; break; }
            released=true;
        }
        try {
            check_context();
            if (camera.owns_rendering_camera()) {
                camera.hold(request);
                draw_cross(request);
            }
            UI::HIDE_HUD_AND_RADAR_THIS_FRAME();
        } catch (...) { /* Still give the add-on a bounded chance to drain. */ }
        WAIT(0);
        ++waits;
    }
    const bool restored=camera.restore();
    return AdapterResult::failure(error+"; story_request_id="+std::to_string(request_id)+
        "; drain="+drain+"; private_camera_destroyed="+(restored?"true":"false")+
        "; shared capture block was not cleared");
}

}  // namespace diogenes::gta
