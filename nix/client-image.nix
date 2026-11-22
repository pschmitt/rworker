{
  bashInteractive,
  cacert,
  coreutils,
  dockerTools,
  openssh,
  rsync,
  rworker,
}:
dockerTools.buildLayeredImage {
  name = "ghcr.io/pschmitt/rworker";
  tag = "latest";
  contents = [
    bashInteractive
    coreutils
    dockerTools.binSh
    dockerTools.usrBinEnv
    dockerTools.fakeNss
    cacert
    openssh
    rsync
    rworker
  ];
  extraCommands = ''
    mkdir -p root tmp
    chmod 0700 root
    chmod 1777 tmp
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
      "org.opencontainers.image.description" = "rworker client for CI jobs";
      "org.opencontainers.image.licenses" = "GPL-3.0-only";
    };
  };
}
