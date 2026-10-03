{
  description = "LNURLcash (LUD-25) bearer-note mint that is its own Lightning node (LDK + BDK)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        rec {
          lnurl-mint = pkgs.callPackage ./nix/package.nix { src = self; };
          default = lnurl-mint;
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.lnurl-mint}/bin/lnurl-mint";
        };
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          # cargo and the kernel's C++ build; bitcoind and python for the
          # regtest end-to-end test (scripts/regtest_e2e.py)
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.lnurl-mint ];
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              pkgs.rustfmt
              pkgs.bitcoind
              pkgs.python3
              pkgs.nodejs
            ];
            LNURLCASHKERNEL_BOOST_DIR = "${pkgs.boost.dev}/lib/cmake/Boost-${pkgs.boost.version}";
          };
        }
      );

      nixosModules = {
        lnurl-mint = import ./nix/module.nix { inherit self; };
        default = self.nixosModules.lnurl-mint;
      };

      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          # the package build runs the whole cargo test suite
          inherit (self.packages.${system}) lnurl-mint;

          # cheap: the module merges into a system and renders a sane unit
          module-eval =
            let
              eval = nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  self.nixosModules.lnurl-mint
                  {
                    services.lnurl-mint = {
                      enable = true;
                      baseUrl = "https://mint.example";
                      network = "signet";
                      bitcoind = {
                        rpc = "127.0.0.1:38332";
                        unit = "bitcoind-main.service";
                        cookieFile = "/var/lib/bitcoind-main/signet/.cookie";
                      };
                      lightning = {
                        openFirewall = true;
                        announceAddresses = [ "203.0.113.7:9735" ];
                      };
                      extraGroups = [ "bitcoind-main" ];
                      settings.BASE_FEE_MSAT = 2000;
                    };
                    system.stateVersion = "25.11";
                  }
                ];
              };
              unit = eval.config.systemd.services.lnurl-mint;
            in
            pkgs.runCommand "lnurl-mint-module-eval" { } ''
              unit=${
                pkgs.writeText "unit.json" (
                  builtins.toJSON {
                    inherit (unit) serviceConfig environment after;
                    ports = eval.config.networking.firewall.allowedTCPPorts;
                  }
                )
              }
              grep -q '"StateDirectory":"lnurl-mint"' $unit
              grep -q '"DynamicUser":true' $unit
              grep -q '"SupplementaryGroups":\["bitcoind-main"\]' $unit
              grep -q '"BASE_URL":"https://mint.example"' $unit
              grep -q '"NETWORK":"signet"' $unit
              grep -q '"BITCOIND_RPC":"127.0.0.1:38332"' $unit
              grep -q '"BITCOIND_RPC_COOKIE":"/var/lib/bitcoind-main/signet/.cookie"' $unit
              grep -q '"BASE_FEE_MSAT":"2000"' $unit
              grep -q '"LN_ANNOUNCE_ADDRESSES":"203.0.113.7:9735"' $unit
              grep -q '"bitcoind-main.service"' $unit
              grep -q '"ports":\[9735\]' $unit
              touch $out
            '';

          # the real proof: a VM with bitcoind on regtest and the mint as its
          # own Lightning node
          vm-regtest = pkgs.testers.nixosTest {
            name = "lnurl-mint-regtest";
            nodes.machine =
              { ... }:
              {
                imports = [ self.nixosModules.lnurl-mint ];
                services.bitcoind.regtest = {
                  enable = true;
                  # regtest keeps its cookie in a 0700 subdirectory: a
                  # user and password instead
                  extraConfig = ''
                    regtest=1
                    fallbackfee=0.0002
                    rpcuser=mint
                    rpcpassword=vm-test
                  '';
                };
                services.lnurl-mint = {
                  enable = true;
                  baseUrl = "http://localhost:8111";
                  network = "regtest";
                  bitcoind = {
                    rpc = "127.0.0.1:18443";
                    unit = "bitcoind-regtest.service";
                  };
                  settings.ADMIN_LISTEN = "127.0.0.1:8112";
                  environmentFiles = [ "/etc/lnurl-mint.env" ];
                };
                environment.etc."lnurl-mint.env".text = ''
                  ADMIN_TOKEN=vm-test
                  BITCOIND_RPC_USER=mint
                  BITCOIND_RPC_PASSWORD=vm-test
                '';
                environment.systemPackages = [ pkgs.bitcoind ];
              };
            testScript = ''
              cli = "bitcoin-cli -regtest -rpcuser=mint -rpcpassword=vm-test"
              admin = "curl -sf -H 'Authorization: Bearer vm-test' http://127.0.0.1:8112"

              machine.wait_for_unit("bitcoind-regtest.service")
              machine.wait_for_unit("lnurl-mint.service")
              machine.wait_for_open_port(8111)
              machine.wait_until_succeeds(f"{admin}/info | grep '\"lightning\":\"ready\"'")

              # the payRequest, and an invoice from the node itself
              machine.succeed("curl -sf http://localhost:8111/.well-known/lnurlp/_ | grep withdrawLink")
              h = "00" * 32
              machine.succeed(f"curl -sf 'http://localhost:8111/p/cb?amount=50000&comment={h}' | grep lnbcrt")

              # the on-chain wallet follows bitcoind's blocks
              machine.succeed(f"{cli} createwallet miner")
              address = machine.succeed(f"{admin}/node/address | sed 's/.*\"address\":\"\\([^\"]*\\)\".*/\\1/'").strip()
              machine.succeed(f"{cli} generatetoaddress 101 $({cli} getnewaddress)")
              machine.succeed(f"{cli} -rpcwallet=miner sendtoaddress {address} 1")
              machine.succeed(f"{cli} generatetoaddress 1 $({cli} getnewaddress)")
              machine.wait_until_succeeds(f"{admin}/node/balance | grep '\"confirmed_sat\":100000000'")

              # a graceful restart keeps the node's identity
              node_id = machine.succeed(f"{admin}/info | sed 's/.*\"mint_pubkey\":\"\\([0-9a-f]*\\)\".*/\\1/'").strip()
              machine.systemctl("restart lnurl-mint.service")
              machine.wait_until_succeeds(f"{admin}/info | grep {node_id}")
            '';
          };
        }
      );
    };
}
