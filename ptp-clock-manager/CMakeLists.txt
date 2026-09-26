cmake_minimum_required(VERSION 3.12)
project(ptp-clock-manager CXX)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# Path to RAVENNA LKM common headers (shared with aes67-linux-daemon)
set(LKM_COMMON_DIR
    "${CMAKE_CURRENT_SOURCE_DIR}/../3rdparty/ravenna-alsa-lkm/common"
    CACHE PATH "RAVENNA LKM common headers directory")
set(LKM_DRIVER_DIR
    "${CMAKE_CURRENT_SOURCE_DIR}/../3rdparty/ravenna-alsa-lkm/driver"
    CACHE PATH "RAVENNA LKM driver headers directory")

find_package(Threads REQUIRED)
find_package(PkgConfig REQUIRED)

option(WITH_ALSA_SRC "Build ALSA SRC driver (requires libasound2, libsoxr)" ON)
option(WITH_PIPEWIRE  "Build PipeWire AES67 driver" OFF)

set(SOURCES
    main.cpp
    ravenna_ptp.cpp
    phc_source.cpp
    media_clock.cpp
    clock_discipline.cpp
    drivers/clock_tai_driver.cpp
    drivers/alsa_src_driver.cpp
    drivers/pipewire_driver.cpp
)

add_executable(ptp-clock-manager ${SOURCES})

target_include_directories(ptp-clock-manager PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}
    ${LKM_COMMON_DIR}
    ${LKM_DRIVER_DIR}
)

target_link_libraries(ptp-clock-manager
    Threads::Threads
    rt    # shm_open, clock_adjtime
)

if(WITH_ALSA_SRC)
    pkg_check_modules(SOXR REQUIRED soxr)
    find_package(ALSA REQUIRED)
    target_compile_definitions(ptp-clock-manager PRIVATE WITH_ALSA_SRC)
    target_include_directories(ptp-clock-manager PRIVATE ${SOXR_INCLUDE_DIRS})
    target_link_libraries(ptp-clock-manager ${ALSA_LIBRARIES} ${SOXR_LIBRARIES})
endif()

if(WITH_PIPEWIRE)
    find_package(PkgConfig REQUIRED)
    pkg_check_modules(PW REQUIRED libpipewire-0.3)
    target_compile_definitions(ptp-clock-manager PRIVATE WITH_PIPEWIRE)
    target_include_directories(ptp-clock-manager PRIVATE ${PW_INCLUDE_DIRS})
    target_link_libraries(ptp-clock-manager ${PW_LIBRARIES})
endif()

install(TARGETS ptp-clock-manager RUNTIME DESTINATION /usr/local/bin)
install(FILES systemd/ptp-clock-manager.service
        DESTINATION /etc/systemd/system)

# Media clock servo test (no framework: asserts, exit code)
add_executable(media-clock-test media_clock_test.cpp media_clock.cpp)
target_include_directories(media-clock-test PRIVATE ${CMAKE_CURRENT_SOURCE_DIR})
