"""Observe libc host-I/O attempts in selected native macOS WireGuard fixtures.

Test instrumentation only: never load this dylib into an installed Mynou.
See docs/host-io-observer.md for the exact coverage and its limits.
"""
import argparse
from collections import Counter
import json
import os
from pathlib import Path
import platform
import subprocess

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = (
    "torrent::tunnel_tests::real_magnet_metadata_payload_and_inbound_seeding_use_only_the_native_network",
    "torrent::tunnel_tests::torrent_http_tracker_and_redirected_remote_metadata_use_virtual_dns_and_http",
    "torrent::tunnel_tests::real_udp_tracker_and_dht_get_peers_and_announce_stay_inside_the_tunnel",
    "torrent::tunnel_tests::https_torrent_tracker_authenticates_the_host_over_the_native_tunnel",
    "tunnel_http::tests::http_dns_and_cross_name_redirect_use_only_encrypted_virtual_services",
    "tunnel_http::tests::unsupported_destinations_and_hostile_redirects_never_connect_directly",
    "tunnel_http::tests::tunnel_http_ignores_environment_proxies",
    "tunnel_http::tests::network::unsupported_destinations_and_names_never_leave_the_tunnel",
    "tunnel_http::tests::network::loss_and_cancellation_fence_every_handle_without_fallback",
    "tunnel_http::tests::cancellation_fences_existing_streams_cloned_connectors_and_inflight_reads",
    "tunnel_http::tests::replaced_generations_refuse_old_connectors_and_prune_live_stale_leases",
    "tunnel_http::tests::loss_observed_inside_a_command_fences_that_commands_result",
    "tunnel_http::tests::network::cancellation_closes_publication_admission_then_waits_for_an_admitted_commit",
    "tunnel_http::tests::network::generation_replacement_waits_for_old_publications_and_rejects_late_results",
    "vpn::tests::real_native_owner_is_used_for_protected_startup_dns_and_revocation",
    "vpn::tests::owner_loss_never_replaces_a_captured_engine_transport_with_direct",
    "vpn::tests::unauthenticated_startup_stays_closed_even_without_download_workers_and_restart_keeps_intent",
    "vpn::tests::verified_bytes_user_pause_and_priorities_survive_owner_loss_and_a_new_protected_engine",
    "vpn::tests::recovered_owner_requires_a_new_engine_and_never_reauthorizes_delayed_old_work",
    "pack::tunnel_tests::shared_pack_preview_and_guarded_apply_use_tunnel_metadata_without_workers",
    "pack::tunnel_tests::cancellation_during_shared_pack_metadata_refuses_preview_apply_and_retry",
    "library::tunnel_tests::group_replacement_preview_and_guarded_apply_use_tunnel_metadata_without_workers",
    "library::tunnel_tests::cancellation_during_group_replacement_metadata_refuses_preview_apply_and_retry",
    "pack::automatic_tunnel_tests::automatic_pack_preview_and_guarded_apply_use_tunnel_metadata_without_workers",
    "pack::automatic_tunnel_tests::cancellation_during_automatic_pack_metadata_refuses_preview_apply_and_retry",
    "tunnel_http::tests::sessions::rekey_concurrent_with_new_operations_keeps_the_generation_and_stream_bytes",
    "tunnel_http::tests::sessions::session_expiry_revokes_new_operations_and_reconnect_needs_a_fresh_single_listener",
    "tunnel_http::tests::sessions::authenticated_roaming_moves_the_outer_endpoint_without_a_new_generation",
    "tunnel_http::tests::sessions::engine_pause_selection_and_verified_bytes_survive_rekey",
    "tunnel_http::tests::sessions::engine_pause_selection_and_verified_bytes_survive_authenticated_roaming",
    "tunnel_http::tests::sessions::engine_pause_selection_and_verified_bytes_survive_expiry_and_replacement",
    "irc::tunnel_tests::irc_announcement_routes_through_tunnel_metadata_without_workers",
    "irc::tunnel_tests::cancellation_during_irc_metadata_refuses_routing_and_later_announcements",
)
NEGATIVE = "torrent::tunnel_tests::host_io_observer_negative_controls"


def validate(rows):
    """Authorize only declared fixture UDP sockets and destinations, never DNS.

    TCP is refused unless a fixture declares an ordinary-path loopback service
    (`allow_tcp`, e.g. an indexer that never enters the tunnel); then only that
    process may listen on or connect to exactly that service port.
    Declarations are made by fixture setup, not derived from intercepted traffic.
    Read the entire trace: setup, shutdown, errors and child processes count.
    """
    allowed = {(r["pid"], r["port"]) for r in rows if r["op"] == "allow"}
    services = {(r["pid"], r["port"]) for r in rows if r["op"] == "allow_tcp"}
    if any(not 0 < port <= 65535 for _, port in services):
        raise ValueError("invalid ordinary service declaration")
    if not allowed or any(not 0 < port <= 65535 for _, port in allowed):
        raise ValueError("missing or invalid explicit outer endpoint declarations")
    loaded = {r["pid"] for r in rows if r["op"] == "loaded"}
    if not loaded:
        raise ValueError("observer was not loaded")
    pending, binding, bound = set(), set(), {}
    streams, stream_binding, connected = set(), set(), set()
    counts = Counter()
    for row in rows:
        op, pid = row["op"], row["pid"]
        if pid not in loaded:
            raise ValueError("event without a loaded observer")
        counts[op] += 1
        key = (pid, row.get("fd"))
        if op in {"loaded", "allow", "allow_tcp"}:
            continue
        if op == "socket":
            connected.discard(key)  # A reused descriptor starts unauthorized.
        if op == "socket" and row["type"] == 1 and any(p == pid for p, _ in services):
            # Darwin AF_INET/SOCK_STREAM, only in a process declaring a service.
            if row["family"] != 2 or row["fd"] < 0 or key in pending or key in streams:
                raise ValueError("unauthorized ordinary socket attempt")
            streams.add(key)
        elif op == "bind" and key in streams:
            if row["family"] != 2 or row["ip"] != "127.0.0.1" or row["port"] != 0:
                raise ValueError("unauthorized ordinary bind attempt")
            stream_binding.add(key)
        elif op == "bound" and key in stream_binding:
            if (pid, row["source"]) not in services:
                raise ValueError("listener bound outside declared ordinary services")
            stream_binding.remove(key)
            streams.remove(key)
            counts["service_listen"] += 1
        elif op == "connect" and key in streams and key not in stream_binding:
            if row["family"] != 2 or row["ip"] != "127.0.0.1" or (pid, row["port"]) not in services:
                raise ValueError("connection outside declared ordinary services")
            streams.remove(key)
            connected.add(key)
            counts["service_connect"] += 1
        elif op == "sendto" and (key in connected or (
                key not in pending and key not in bound and (pid, row["source"]) in services)):
            # Darwin libc sends stream bytes as sendto without an address: from a
            # connected client, or from a socket accepted on the declared port.
            if row["family"] != 0 or row["port"] != 0:
                raise ValueError("connection outside declared ordinary services")
            counts["service_send"] += 1
        elif op == "socket":
            # Darwin AF_INET/SOCK_DGRAM. Errors, IPv6, stream/raw sockets fail.
            if row["family"] != 2 or row["type"] != 2 or row["fd"] < 0 or key in pending:
                raise ValueError("unauthorized ordinary socket attempt")
            pending.add(key)
        elif op == "bind":
            if key not in pending or row["family"] != 2 or row["ip"] != "127.0.0.1" or row["port"] != 0:
                raise ValueError("unauthorized ordinary bind attempt")
            binding.add(key)
        elif op == "bound":
            if key not in binding or (pid, row["source"]) not in allowed:
                raise ValueError("socket bound outside declared outer endpoints")
            binding.remove(key)
            pending.remove(key)
            bound[key] = row["source"]
        elif op == "sendto":
            if (row["family"] != 2 or row["ip"] != "127.0.0.1"
                    or bound.get(key) != row["source"]
                    or (pid, row["source"]) not in allowed or (pid, row["port"]) not in allowed):
                raise ValueError("datagram outside declared outer endpoints")
        else:
            # Includes connect, getaddrinfo and sendmsg, even if their call fails.
            raise ValueError("unauthorized ordinary attempt: " + op)
    if (pending or binding or streams or stream_binding
            or not counts["socket"] or not counts["sendto"]):
        raise ValueError("incomplete socket/outer-datagram observation")
    return dict(sorted(counts.items()))


def read_trace(path):
    if path.stat().st_size > 128 * 1024 * 1024:
        raise ValueError("observation exceeds fixture trace budget")
    return [json.loads(line) for line in path.read_text().splitlines()]


def harness(build):
    candidates = [row["executable"] for row in map(json.loads, build.read_text().splitlines())
                  if row.get("executable") and row.get("target", {}).get("kind") == ["lib"]
                  and row.get("profile", {}).get("test")]
    if len(set(candidates)) != 1:
        raise ValueError("expected one compiled library test harness")
    return str(Path(candidates[0]).resolve())


def capture(command, name, library, output, extra_env=None):
    trace = output / (name + ".jsonl")
    trace.touch(mode=0o600, exist_ok=False)
    env = {**os.environ, "DYLD_INSERT_LIBRARIES": str(library), "MYNOU_HOST_IO_TRACE": str(trace)}
    env.update(extra_env or {})
    result = subprocess.run(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            timeout=120, check=False)
    (output / (name + ".log")).write_bytes(result.stdout)
    if result.returncode:
        raise ValueError(f"fixture {name} failed ({result.returncode}); see its log")
    return read_trace(trace)


def failed_lookup_control(library, output):
    # AI_NUMERICHOST makes failure deterministic without an external DNS query.
    source = output / "failed_lookup.c"
    source.write_text('''#include <netdb.h>
int main(void) {
    struct addrinfo hints = {0}, *answer = 0;
    hints.ai_flags = AI_NUMERICHOST;
    return getaddrinfo("not-a-numeric-address", "9", &hints, &answer) == 0;
}
''')
    executable = output / "failed_lookup"
    subprocess.run(["clang", "-Wall", "-Wextra", "-Werror", str(source), "-o", str(executable)], check=True)
    rows = capture([str(executable)], "negative-failed-lookup", library, output)
    if Counter(r["op"] for r in rows)["getaddrinfo"] != 1:
        raise ValueError("failed libc name lookup was not observed")
    return {"failed_getaddrinfo": "observed"}


def run(build, output):
    if (platform.system(), platform.machine()) != ("Darwin", "arm64"):
        raise ValueError("host-I/O observation requires native macOS arm64")
    output.mkdir(parents=True, exist_ok=True)
    executable = harness(build)
    library = output / "host_io_observer.dylib"
    subprocess.run(["clang", "-dynamiclib", "-Wall", "-Wextra", "-Werror", "-pthread",
                    str(ROOT / ".github/scripts/host_io_observer.c"), "-o", str(library)], check=True)
    # Ensure exact test filters cannot silently execute zero tests.
    listing = subprocess.check_output([executable, "--list"], text=True)
    for name in (*FIXTURES, NEGATIVE):
        if name + ": test\n" not in listing:
            raise ValueError("missing fixture: " + name)
    summary = {"platform": "macOS arm64", "fixtures": {}, "negative_controls": {}}
    rows = capture([executable, "--exact", NEGATIVE, "--nocapture"],
                   "negative-rust-stdlib", library, output)
    counts = Counter(r["op"] for r in rows)
    for op in ("loaded", "socket", "bind", "connect", "sendto", "getaddrinfo"):
        if not counts[op]:
            raise ValueError("Rust stdlib negative control did not observe " + op)
    # Give the validator a declaration so rejection cannot be just empty allowlist.
    declared = rows + [{"op": "allow", "pid": rows[0]["pid"], "port": 9}]
    try:
        validate(declared)
    except ValueError:
        pass
    else:
        raise ValueError("validator accepted the deliberate ordinary-network bypass")
    summary["negative_controls"]["rust_stdlib"] = dict(counts)
    summary["negative_controls"].update(failed_lookup_control(library, output))
    for index, fixture in enumerate(FIXTURES):
        rows = capture([executable, "--exact", fixture, "--nocapture"],
                       f"fixture-{index:02d}", library, output)
        summary["fixtures"][fixture] = validate(rows)
        services = summary["fixtures"][fixture].get("service_connect", 0)
        print("Observed only authorized outer UDP"
              + (f" and {services} declared ordinary service connections" if services else "")
              + ": " + fixture, flush=True)
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-json", type=Path, required=True)
    parser.add_argument("--output", type=lambda s: Path(s).resolve(), required=True)
    args = parser.parse_args()
    run(args.build_json, args.output)


if __name__ == "__main__":
    main()
