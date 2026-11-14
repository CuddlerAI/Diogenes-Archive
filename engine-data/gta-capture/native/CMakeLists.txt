cmake_minimum_required(VERSION 3.20)
project(diogenes_gta_controller VERSION 0.1.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

add_library(gta_controller_core STATIC
    src/controller_core.cpp
    src/mock_game_adapter.cpp
    src/process_runtime.cpp
)
target_include_directories(gta_controller_core PUBLIC include)
target_compile_definitions(gta_controller_core PUBLIC UNICODE _UNICODE)

if(MSVC)
    target_compile_options(gta_controller_core PRIVATE /W4 /permissive- /EHsc)
else()
    target_compile_options(gta_controller_core PRIVATE -Wall -Wextra -Wpedantic)
endif()

add_executable(gta_controller_mock src/mock_main.cpp)
target_link_libraries(gta_controller_mock PRIVATE gta_controller_core)

# native_tape.cpp is std-only and portable; building its reader on the host lets
# CTest pin the camera-tape.bin layout against the Python writer's committed
# golden fixture instead of trusting hand-matched constants on both sides.
add_executable(gta_native_tape_selftest
    src/native_tape_selftest.cpp
    src/native_tape.cpp
)
target_include_directories(gta_native_tape_selftest PRIVATE include)
if(MSVC)
    target_compile_options(gta_native_tape_selftest PRIVATE /W4 /permissive- /EHsc)
else()
    target_compile_options(gta_native_tape_selftest PRIVATE -Wall -Wextra -Wpedantic)
endif()

enable_testing()
add_test(NAME gta_controller_protocol_selftest COMMAND gta_controller_mock --self-test)

file(SHA256 "${CMAKE_CURRENT_SOURCE_DIR}/../tests/fixtures/golden-camera-tape.json"
     DIOGENES_GOLDEN_TAPE_JSON_SHA256)
add_test(NAME gta_native_tape_golden_contract COMMAND gta_native_tape_selftest
    "${CMAKE_CURRENT_SOURCE_DIR}/../tests/fixtures/golden-camera-tape.bin"
    "${DIOGENES_GOLDEN_TAPE_JSON_SHA256}")

add_executable(gta_process_runtime_selftest src/process_runtime_selftest.cpp)
target_link_libraries(gta_process_runtime_selftest PRIVATE gta_controller_core)
add_test(NAME gta_process_runtime_lifecycle_selftest COMMAND gta_process_runtime_selftest)

add_executable(gta_editor_export_plan_selftest
    src/editor_export_plan_selftest.cpp
    src/editor_export_plan.cpp
)
target_include_directories(gta_editor_export_plan_selftest PRIVATE include)
if(MSVC)
    target_compile_options(gta_editor_export_plan_selftest PRIVATE /W4 /permissive- /EHsc)
else()
    target_compile_options(gta_editor_export_plan_selftest PRIVATE -Wall -Wextra -Wpedantic)
endif()
add_test(NAME gta_editor_export_plan_contract COMMAND gta_editor_export_plan_selftest)

add_executable(gta_story_route_selftest src/story_route_selftest.cpp)
target_include_directories(gta_story_route_selftest PRIVATE include)
if(MSVC)
    target_compile_options(gta_story_route_selftest PRIVATE /W4 /permissive- /EHsc)
else()
    target_compile_options(gta_story_route_selftest PRIVATE -Wall -Wextra -Wpedantic)
endif()
add_test(NAME gta_story_route_contract COMMAND gta_story_route_selftest)

add_executable(gta_recording_camera_selftest src/recording_camera_selftest.cpp)
target_include_directories(gta_recording_camera_selftest PRIVATE include)
if(MSVC)
    target_compile_options(gta_recording_camera_selftest PRIVATE /W4 /permissive- /EHsc)
else()
    target_compile_options(gta_recording_camera_selftest PRIVATE -Wall -Wextra -Wpedantic)
endif()
add_test(NAME gta_recording_camera_contract COMMAND gta_recording_camera_selftest)

if(WIN32)
    add_executable(gta_editor_identity_observer_selftest
        src/editor_identity_observer_selftest.cpp
        src/fx_capture_observer_win.cpp
    )
    target_include_directories(gta_editor_identity_observer_selftest PRIVATE include)
    target_compile_definitions(gta_editor_identity_observer_selftest PRIVATE
        UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX
        DIOGENES_EDITOR_IDENTITY_OBSERVER_TEST_HOOK)
    target_compile_options(gta_editor_identity_observer_selftest PRIVATE
        /W4 /permissive- /EHsc)
    add_test(NAME gta_editor_identity_observer_access_selftest
        COMMAND gta_editor_identity_observer_selftest)

    add_executable(gta_hash_utils_selftest
        src/hash_utils_selftest.cpp
        src/hash_utils_win.cpp
    )
    target_include_directories(gta_hash_utils_selftest PRIVATE include)
    target_compile_definitions(gta_hash_utils_selftest PRIVATE
        UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX)
    target_compile_options(gta_hash_utils_selftest PRIVATE /W4 /permissive- /EHsc)
    target_link_libraries(gta_hash_utils_selftest PRIVATE bcrypt)
    add_test(NAME gta_hash_utils_small_stack_selftest COMMAND gta_hash_utils_selftest)
endif()

# stage_plan.cpp is std-only: the actors' plan (paths, beats, snap timing,
# calibration files) is pinned on the host, without the game.
add_executable(gta_stage_plan_selftest
    src/stage_plan_selftest.cpp
    src/stage_plan.cpp
)
target_include_directories(gta_stage_plan_selftest PRIVATE include)
if(MSVC)
    target_compile_options(gta_stage_plan_selftest PRIVATE /W4 /permissive- /EHsc /utf-8)
else()
    target_compile_options(gta_stage_plan_selftest PRIVATE -Wall -Wextra -Wpedantic)
endif()
add_test(NAME gta_stage_plan_contract COMMAND gta_stage_plan_selftest
    "${CMAKE_CURRENT_SOURCE_DIR}/../examples/stage-beach-demo.txt")

# What a stage spec puts where, tick by tick, without the game: the input of
# `python -m gta_capture stage-spec-check` (gta_capture/stage_check.py).
add_executable(gta_stage_plan_dump
    src/stage_plan_dump.cpp
    src/stage_plan.cpp
)
target_include_directories(gta_stage_plan_dump PRIVATE include)
if(MSVC)
    target_compile_options(gta_stage_plan_dump PRIVATE /W4 /permissive- /EHsc /utf-8)
else()
    target_compile_options(gta_stage_plan_dump PRIVATE -Wall -Wextra -Wpedantic)
endif()

option(DIOGENES_BUILD_SCRIPT_HOOK_V_ASI "Build the Windows ScriptHook V ASI target" OFF)
set(SCRIPT_HOOK_V_SDK_ROOT "" CACHE PATH "Root of the ScriptHook V SDK (must contain inc/ and lib/)")

if(DIOGENES_BUILD_SCRIPT_HOOK_V_ASI)
    if(NOT WIN32)
        message(FATAL_ERROR "The ScriptHook V ASI target is Windows-only")
    endif()
    find_path(SCRIPT_HOOK_V_INCLUDE_DIR main.h
        PATHS "${SCRIPT_HOOK_V_SDK_ROOT}/inc"
        NO_DEFAULT_PATH REQUIRED)
    find_library(SCRIPT_HOOK_V_LIBRARY NAMES ScriptHookV
        PATHS "${SCRIPT_HOOK_V_SDK_ROOT}/lib"
        NO_DEFAULT_PATH REQUIRED)

    add_library(DiogenesGtaController SHARED
        src/asi_main.cpp
        src/pipe_server_win.cpp
        src/scripthook_game_adapter.cpp
        src/stage_plan.cpp
        src/stage_runtime_win.cpp
        src/stage_take_win.cpp
        src/story_projection.cpp
        src/fx_capture_observer_win.cpp
        src/hash_utils_win.cpp
        src/native_tape.cpp
        src/editor_export_plan.cpp
        src/controller_core.cpp
        src/process_runtime.cpp
    )
    target_include_directories(DiogenesGtaController PRIVATE include "${SCRIPT_HOOK_V_INCLUDE_DIR}")
    target_link_libraries(DiogenesGtaController PRIVATE
        "${SCRIPT_HOOK_V_LIBRARY}" bcrypt Shell32 Ole32)
    target_compile_definitions(DiogenesGtaController PRIVATE
        UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX)
    # ScriptHookV's header-only native table intentionally contains thousands of
    # unused wrappers. Keep our warnings strict without flooding the build with
    # SDK-owned C4505/C4127 diagnostics. /utf-8: the take's causal records are
    # Chinese, and MSVC otherwise reads sources in the machine's code page.
    target_compile_options(DiogenesGtaController PRIVATE
        /W4 /permissive- /EHsc /wd4505 /wd4127 /utf-8)
    set_property(TARGET DiogenesGtaController PROPERTY
        MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
    set_target_properties(DiogenesGtaController PROPERTIES
        PREFIX ""
        SUFFIX ".asi"
        OUTPUT_NAME "DiogenesGtaController"
    )
endif()
