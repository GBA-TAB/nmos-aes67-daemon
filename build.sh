#!/bin/bash
#
# Tested on Ubuntu 18.04
#

#we need clang when compiling on ARMv7
#export CC=/usr/bin/clang
#export CXX=/usr/bin/clang++

TOPDIR=$(pwd)

echo "Init git submodules ..."
git submodule update --init --recursive

cd 3rdparty/ravenna-alsa-lkm/driver
# The submodule's own default remote (see ../../../.gitmodules) is upstream bondagit/ravenna-alsa-lkm --
# left as-is so it stays easy to track upstream. The driver actually built here comes from our own
# fork instead, which carries changes upstream doesn't have yet (currently: real per-leg PTP status
# for SMPTE 2022-7 redundancy, BCP-008 TX stream status bits, and a raised ALSA channel ceiling).
git remote get-url fork >/dev/null 2>&1 || git remote add fork https://github.com/GBA-TAB/ravenna-alsa-lkm.git
git fetch fork
git checkout -B experimental-hw-timestamping fork/experimental-hw-timestamping
make
cd -

cd webui
echo "Downloading current webui release ..."
wget --timestamping https://github.com/bondagit/aes67-linux-daemon/releases/latest/download/webui.tar.gz
if [ -f webui.tar.gz ]; then
  tar -xzvf webui.tar.gz
else
  echo "Building and installing webui ..."
  # npm install react-modal react-toastify react-router-dom
  npm ci
  npm run build
fi
cd ..

cd daemon

echo "Building aes67-daemon ..."
cmake \
	-DBoost_NO_WARN_NEW_VERSIONS=1 \
	-DCPP_HTTPLIB_DIR="${TOPDIR}/3rdparty/cpp-httplib" \
	-DRAVENNA_ALSA_LKM_DIR="${TOPDIR}/3rdparty/ravenna-alsa-lkm" \
	-DENABLE_TESTS=ON \
	-DWITH_AVAHI=ON \
	-DFAKE_DRIVER=OFF \
	-DWITH_SYSTEMD=ON \
	-DWITH_STREAMER=ON \
	-DWITH_NMOS=ON \
	.
make
cd ..
cd test
make
cd ..

