#!/bin/sh
# Regenerates the package-manager logs in this directory from throwaway
# containers: real dpkg, apt, dnf and yum runs, nothing vendored.
#
# Needs docker (run with sudo where docker does). Run from anywhere:
#   sudo sh tests/fixtures/gen.sh
#
# Each container starts with empty logs, does a known set of installs and
# removals, and its logs are copied out. Containers are named `builder` and
# the non-root account is `analyst`, so nothing about the machine that ran
# this ends up in the files. Times are whenever this ran; the tests check
# what was done, not when.
#
# CentOS 7 is end of life: its repositories are read from vault.centos.org.
set -eu

here=$(cd "$(dirname "$0")" && pwd)

debian() {
    out="$here/debian"
    mkdir -p "$out"
    # Europe/Paris: dpkg writes local wall-clock times, apt too.
    docker run --rm --hostname builder -e TZ=Europe/Paris \
        -v "$out:/out" debian:trixie sh -euc '
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        apt-get install -y -qq sudo tzdata >/dev/null
        ln -sf /usr/share/zoneinfo/Europe/Paris /etc/localtime
        : > /var/log/dpkg.log
        : > /var/log/apt/history.log
        useradd -m analyst
        echo "analyst ALL=(ALL) NOPASSWD: ALL" > /etc/sudoers.d/analyst
        apt-get install -y -qq tree jq >/dev/null
        su analyst -c "sudo apt-get install -y -qq hello" >/dev/null
        apt-get install -y -qq --reinstall hello >/dev/null
        apt-get remove -y -qq tree >/dev/null
        apt-get purge -y -qq jq >/dev/null
        apt-get autoremove -y -qq --purge >/dev/null
        cp /var/log/dpkg.log /out/dpkg.log
        mkdir -p /out/apt && cp /var/log/apt/history.log /out/apt/history.log
    '
}

rocky() {
    out="$here/rocky9"
    mkdir -p "$out"
    # Asia/Kolkata: dnf writes the local time with its offset (+0530).
    docker run --rm --hostname builder -e TZ=Asia/Kolkata \
        -v "$out:/out" rockylinux:9 sh -euc '
        : > /var/log/dnf.rpm.log
        dnf install -y -q tree jq >/dev/null
        dnf reinstall -y -q tree >/dev/null
        dnf remove -y -q tree >/dev/null
        dnf remove -y -q jq >/dev/null
        cp /var/log/dnf.rpm.log /out/dnf.rpm.log
    '
}

# dnf's history database: four transactions, one run through sudo by a
# user (a container records no login uid, so it is -1 there). The database
# is copied after dnf exits, its write-ahead log checkpointed into it.
dnf_history() {
    out="$here/rocky9/dnf"
    mkdir -p "$out"
    docker run --rm --hostname builder -v "$out:/out" rockylinux:9 sh -euc '
        useradd -m -u 1000 analyst
        dnf install -y -q sudo >/dev/null
        echo "analyst ALL=(ALL) NOPASSWD: ALL" > /etc/sudoers.d/analyst
        dnf install -y -q tree >/dev/null
        su - analyst -c "sudo dnf install -y -q jq" >/dev/null
        dnf remove -y -q tree >/dev/null
        cp /var/lib/dnf/history.sqlite /out/history.sqlite
    '
}

centos() {
    out="$here/centos7"
    mkdir -p "$out"
    docker run --rm --hostname builder \
        -v "$out:/out" centos:7 sh -euc '
        sed -i -e "s/^mirrorlist=/#mirrorlist=/" \
            -e "s|^#baseurl=http://mirror.centos.org|baseurl=http://vault.centos.org|" \
            /etc/yum.repos.d/CentOS-*.repo
        : > /var/log/yum.log
        yum install -y -q tree bc >/dev/null
        yum reinstall -y -q tree >/dev/null
        yum remove -y -q tree >/dev/null
        yum erase -y -q bc >/dev/null
        cp /var/log/yum.log /out/yum.log
    '
}

debian
rocky
dnf_history
centos
# The files belong to whoever ran this, not to root.
chown -R "${SUDO_UID:-$(id -u)}:${SUDO_GID:-$(id -g)}" "$here"
