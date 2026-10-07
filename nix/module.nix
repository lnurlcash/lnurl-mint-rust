{ self }:

{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.lnurl-mint;
  inherit (lib)
    mkEnableOption
    mkOption
    mkIf
    types
    ;

  toEnvValue = v: if builtins.isBool v then (if v then "true" else "false") else toString v;
in
{
  options.services.lnurl-mint = {
    enable = mkEnableOption "lnurl-mint, an LNURLcash (LUD-25) bearer-note mint that is its own Lightning node";

    package = lib.mkPackageOption self.packages.${pkgs.stdenv.hostPlatform.system} "lnurl-mint" { };

    baseUrl = mkOption {
      type = types.str;
      example = "https://mint.example.com";
      description = ''
        BASE_URL: the mint's public URL. Its host is the domain `ck1` spends
        bind to, and the mint is payable at `<username>@<that host>`.
      '';
    };

    listen = mkOption {
      type = types.str;
      default = "127.0.0.1:8111";
      description = ''
        LISTEN: where the LNURL endpoints are served. Loopback by default:
        front it with a TLS reverse proxy (or a Tor HiddenServiceDir).
      '';
    };

    network = mkOption {
      type = types.enum [
        "bitcoin"
        "testnet"
        "testnet4"
        "signet"
        "regtest"
      ];
      default = "bitcoin";
      description = "NETWORK: the chain the node runs on; bitcoind must be on the same one.";
    };

    bitcoind = {
      rpc = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "127.0.0.1:8332";
        description = ''
          BITCOIND_RPC: host or host:port of the bitcoind the node and its
          wallet sync from. null runs the mint without a Lightning node:
          rotate, split and merge work, minting and melting don't.
        '';
      };

      unit = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "bitcoind-main.service";
        description = "A local bitcoind's systemd unit, to start after (and with).";
      };

      cookieFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        example = "/var/lib/bitcoind-main/.cookie";
        description = ''
          BITCOIND_RPC_COOKIE: bitcoind's cookie file. bitcoind writes it
          0600 by default; give it `rpccookieperms=group` and add its group to
          `extraGroups`. That works on mainnet, where the cookie sits in the
          data directory itself. On test networks bitcoind keeps it in a 0700
          subdirectory: there, and without a cookie at all, put
          BITCOIND_RPC_USER and BITCOIND_RPC_PASSWORD in an environmentFile.
        '';
      };
    };

    lightning = {
      listen = mkOption {
        type = types.str;
        default = "0.0.0.0:9735";
        description = "LN_LISTEN: where peers reach the node.";
      };

      openFirewall = mkOption {
        type = types.bool;
        default = false;
        description = "Open the peer port in the firewall.";
      };

      alias = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "LN_ALIAS: the alias announced with public channels (default: TITLE).";
      };

      announceAddresses = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "203.0.113.7:9735" ];
        description = "LN_ANNOUNCE_ADDRESSES: host:port addresses announced with public channels.";
      };
    };

    dataDir = mkOption {
      type = types.path;
      default = "/var/lib/lnurl-mint";
      description = ''
        DATA_DIR: the seed, the node's channel state, the on-chain wallet and
        the note database. Back it up as a whole, and never restore an old
        copy while channels are open. Created via systemd's StateDirectory
        (mode 0700) when left at the default.
      '';
    };

    extraGroups = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "bitcoind-main" ];
      description = "Supplementary groups, for reading bitcoind's cookie file.";
    };

    settings = mkOption {
      type = types.attrsOf (
        types.oneOf [
          types.str
          types.int
          types.bool
        ]
      );
      default = { };
      example = {
        BASE_FEE_MSAT = 1000;
        ONION_URL = "http://<v3-address>.onion";
        ADMIN_LISTEN = "127.0.0.1:8112";
      };
      description = ''
        Any other environment variable (see .env.example), rendered 1:1 as
        KEY=value. Dedicated options win over a raw key naming the same
        variable. Non-secret values only: ADMIN_TOKEN and RPC passwords go in
        environmentFiles, out of the world-readable nix store.
      '';
    };

    environmentFiles = mkOption {
      type = types.listOf types.path;
      default = [ ];
      example = [ "/run/secrets/lnurl-mint" ];
      description = ''
        systemd EnvironmentFile(s) for secrets: ADMIN_TOKEN (the admin API is
        off without it), BITCOIND_RPC_USER/BITCOIND_RPC_PASSWORD. Loaded
        after everything else, so values here win.
      '';
    };
  };

  config = mkIf cfg.enable {
    # lnurl-mint-cli, for `sudo lnurl-mint-cli --data-dir /var/lib/lnurl-mint info`:
    # the service's dynamic user owns the admin socket, and root may use it
    environment.systemPackages = [ cfg.package ];

    networking.firewall.allowedTCPPorts = mkIf cfg.lightning.openFirewall [
      (lib.toInt (lib.last (lib.splitString ":" cfg.lightning.listen)))
    ];

    systemd.services.lnurl-mint = {
      description = "lnurl-mint LNURLcash bearer-note mint";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ] ++ lib.optional (cfg.bitcoind.unit != null) cfg.bitcoind.unit;
      wants = [ "network-online.target" ];
      requires = lib.optional (cfg.bitcoind.unit != null) cfg.bitcoind.unit;

      environment =
        (lib.mapAttrs (_: toEnvValue) cfg.settings)
        // {
          BASE_URL = cfg.baseUrl;
          LISTEN = cfg.listen;
          NETWORK = cfg.network;
          DATA_DIR = cfg.dataDir;
          LN_LISTEN = cfg.lightning.listen;
        }
        // lib.optionalAttrs (cfg.bitcoind.rpc != null) { BITCOIND_RPC = cfg.bitcoind.rpc; }
        // lib.optionalAttrs (cfg.bitcoind.cookieFile != null) {
          BITCOIND_RPC_COOKIE = toString cfg.bitcoind.cookieFile;
        }
        // lib.optionalAttrs (cfg.lightning.alias != null) { LN_ALIAS = cfg.lightning.alias; }
        // lib.optionalAttrs (cfg.lightning.announceAddresses != [ ]) {
          LN_ANNOUNCE_ADDRESSES = lib.concatStringsSep "," cfg.lightning.announceAddresses;
        };

      serviceConfig = {
        ExecStart = lib.getExe cfg.package;
        EnvironmentFile = cfg.environmentFiles;
        Restart = "on-failure";
        RestartSec = 5;
        # SIGTERM stops the node gracefully, writing its channel state; a
        # SIGKILL after the timeout can make LDK force-close a channel
        TimeoutStopSec = 120;

        DynamicUser = true;
        SupplementaryGroups = cfg.extraGroups;
        ReadWritePaths = [ cfg.dataDir ];
        UMask = "0077";
        # the mint holds a hot wallet's seed and its channels - lock it down
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        RestrictSUIDSGID = true;
        RestrictNamespaces = true;
        RestrictRealtime = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        CapabilityBoundingSet = "";
        # HTTP in, peers in and out, bitcoind RPC out
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
      }
      // lib.optionalAttrs (cfg.dataDir == "/var/lib/lnurl-mint") {
        StateDirectory = "lnurl-mint";
        StateDirectoryMode = "0700";
      };
    };
  };
}
