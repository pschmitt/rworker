{
  bashInteractive,
  cacert,
  coreutils,
  cryptsetup,
  curl,
  dockerTools,
  e2fsprogs,
  git,
  gnugrep,
  nix,
  openssh,
  openssl,
  procps,
  python3,
  rsync,
  util-linux,
}:
dockerTools.buildLayeredImage {
  name = "ghcr.io/pschmitt/rworker-lease";
  tag = "latest";
  includeNixDB = true;
  contents = [
    bashInteractive
    cacert
    coreutils
    cryptsetup
    curl
    dockerTools.binSh
    dockerTools.usrBinEnv
    e2fsprogs
    git
    gnugrep
    nix
    openssh
    openssl.bin
    procps
    python3
    rsync
  ];
  extraCommands = ''
    mkdir -p etc/nix etc/ssh etc/ssl/certs etc/rworker home/rworker-build root tmp var/empty usr/sbin
    touch etc/rworker/lease-image
    chmod 0700 root home/rworker-build
    chmod 1777 tmp
    chmod 0555 var/empty
    ln -s ${openssh}/bin/sshd usr/sbin/sshd
    ln -s ${util-linux}/bin/mount bin/mount
    ln -s ${util-linux}/bin/umount bin/umount
    ln -s ca-bundle.crt etc/ssl/certs/ca-certificates.crt
    cat > etc/passwd <<'USERS'
    root:x:0:0:root:/root:/bin/bash
    rworker-build:x:1000:1000:Worker:/home/rworker-build:/bin/bash
    sshd:x:1001:1001:SSH privilege separation:/var/empty:/bin/sh
    nixbld1:x:1101:1100:Nix build user:/var/empty:/bin/sh
    nixbld2:x:1102:1100:Nix build user:/var/empty:/bin/sh
    nixbld3:x:1103:1100:Nix build user:/var/empty:/bin/sh
    nixbld4:x:1104:1100:Nix build user:/var/empty:/bin/sh
    USERS
    cat > etc/group <<'GROUPS'
    root:x:0:
    rworker-build:x:1000:
    sshd:x:1001:
    nixbld:x:1100:nixbld1,nixbld2,nixbld3,nixbld4
    GROUPS
    cat > etc/shadow <<'SHADOW'
    root:!:1:0:99999:7:::
    rworker-build:*:1:0:99999:7:::
    sshd:!:1:0:99999:7:::
    SHADOW
    chmod 0600 etc/shadow
    cat > etc/nsswitch.conf <<'NSS'
    passwd: files
    group: files
    shadow: files
    hosts: files dns
    NSS
    cat > etc/nix/nix.conf <<'NIX'
    experimental-features = nix-command flakes
    build-users-group = nixbld
    sandbox = false
    allowed-users = root rworker-build
    ssl-cert-file = /etc/ssl/certs/ca-bundle.crt
    NIX
  '';
  fakeRootCommands = ''
    chown 1000:1000 home/rworker-build
  '';
  config = {
    Cmd = [ "/bin/bash" ];
    Env = [
      "HOME=/root"
      "PATH=/bin"
      "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
    ];
    Labels = {
      "org.opencontainers.image.source" = "https://github.com/pschmitt/rworker";
      "org.opencontainers.image.description" = "rworker GitLab CI lease runtime";
    };
  };
}
