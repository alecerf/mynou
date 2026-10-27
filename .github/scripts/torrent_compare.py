"""Opt-in local torrent comparison. Dependencies, builds and payloads belong in /tmp.

Requires the libtorrent Python binding and optionally an rqbit binary. No public
swarm, tracker, DHT, UPnP or system VPN is used. See docs/torrent-benchmark.md.
"""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import re
import signal
import subprocess
import sys
import time


def session(encryption="negotiated"):
    import libtorrent as lt
    settings = {
        "listen_interfaces": "127.0.0.1:0", "enable_dht": False,
        "enable_lsd": False, "enable_upnp": False, "enable_natpmp": False,
        "enable_incoming_utp": False, "enable_outgoing_utp": False,
        "allow_multiple_connections_per_ip": True,
        "announce_to_all_trackers": False,
        "ignore_limits_on_local_network": False,
    }
    if encryption == "disabled":
        settings.update(out_enc_policy=int(lt.enc_policy.disabled),
                        in_enc_policy=int(lt.enc_policy.disabled))
    elif encryption == "forced":
        settings.update(out_enc_policy=int(lt.enc_policy.forced),
                        in_enc_policy=int(lt.enc_policy.forced),
                        allowed_enc_level=int(lt.enc_level.rc4))
    return lt.session(settings)


def lt_download(args):
    import libtorrent as lt
    started = time.monotonic()
    ses = session(args.peer_encryption)
    params = lt.parse_magnet_uri(args.source)
    params.save_path = str(args.root)
    params.flags &= ~lt.torrent_flags.auto_managed
    params.flags &= ~lt.torrent_flags.paused
    handle = ses.add_torrent(params)
    handle.set_download_limit(args.rate)
    for peer in args.peers.split(","):
        host, port = peer.split(":")
        handle.connect_peer((host, int(port)))
    while not handle.status().is_seeding:
        status = handle.status()
        if status.errc.value():
            raise RuntimeError(status.errc.message())
        if time.monotonic() - started > 180:
            raise TimeoutError("libtorrent acquisition timeout")
        time.sleep(0.005)
    print(json.dumps({"ready_s": time.monotonic() - started}), flush=True)
    # The parent verifies file bytes after process exit. This is not a claim of
    # matching Mynou's durable completion/fsync contract.
    del handle
    del ses


class Proxy:
    """Delay each byte once per direction, without a per-chunk throughput cap.

    Reader and sender are independent. Serial sleep-per-read proxies otherwise
    manufacture a bandwidth bottleneck instead of modelling propagation delay.
    Queues and asyncio stream buffers are bounded; TCP supplies backpressure.
    """
    def __init__(self, port, rtt_ms):
        self.port = port
        self.delay = rtt_ms / 2000
        self.tasks = set()
        self.writers = set()
        self.handshakes = {"plaintext": 0, "other": 0}

    async def pump(self, reader, writer, observe=False):
        queue = asyncio.Queue(maxsize=128)
        async def receive():
            prefix = bytearray()
            counted = False
            while True:
                data = await reader.read(65536)
                if observe and not counted:
                    prefix.extend(data[:20 - len(prefix)])
                    if len(prefix) == 20 or not data:
                        key = "plaintext" if prefix == b"\x13BitTorrent protocol" else "other"
                        self.handshakes[key] += 1
                        counted = True
                await queue.put((time.monotonic() + self.delay, data))
                if not data:
                    break
        async def send():
            while True:
                due, data = await queue.get()
                await asyncio.sleep(max(0, due - time.monotonic()))
                if not data:
                    return
                writer.write(data)
                await writer.drain()
        tasks = [asyncio.create_task(receive()), asyncio.create_task(send())]
        try:
            await asyncio.gather(*tasks)
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)

    async def accept(self, reader, writer):
        task = asyncio.current_task()
        self.tasks.add(task)
        self.writers.add(writer)
        remote = None
        try:
            upstream, remote = await asyncio.open_connection("127.0.0.1", self.port)
            self.writers.add(remote)
            tasks = [asyncio.create_task(self.pump(reader, remote, observe=True)),
                     asyncio.create_task(self.pump(upstream, writer))]
            try:
                await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
            finally:
                for child in tasks:
                    child.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)
        except (ConnectionError, OSError):
            pass
        finally:
            writer.close()
            self.writers.discard(writer)
            if remote:
                remote.close()
                self.writers.discard(remote)
            self.tasks.discard(task)

    async def close(self):
        for writer in list(self.writers):
            writer.close()
        for task in list(self.tasks):
            task.cancel()
        await asyncio.gather(*list(self.tasks), return_exceptions=True)


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


async def run(args):
    import libtorrent as lt
    output = args.output.resolve()
    if not output.is_relative_to(Path("/tmp").resolve()):
        raise ValueError("--output must be inside /tmp")
    output.mkdir(parents=True, exist_ok=True)
    seed_root = output / "seed"
    seed_root.mkdir(exist_ok=True)
    payload = seed_root / "payload.bin"
    # Nonzero deterministic data, generated before any measurement.
    block = bytes((i * 137 + 19) % 251 for i in range(1 << 20))
    with payload.open("wb") as stream:
        for _ in range(args.mib):
            stream.write(block)
    expected = digest(payload)
    storage = lt.file_storage()
    storage.add_file("payload.bin", payload.stat().st_size)
    flags = {"v1": lt.create_torrent.v1_only, "v2": lt.create_torrent.v2_only,
             "hybrid": 0}[args.kind]
    creator = lt.create_torrent(storage, 1 << 20, flags)
    lt.set_piece_hashes(creator, str(seed_root))
    torrent = output / "payload.torrent"
    torrent.write_bytes(lt.bencode(creator.generate()))
    info = lt.torrent_info(str(torrent))
    info_hash = str(info.info_hashes().v1 if args.kind == "v1" else info.info_hashes().v2)
    magnet = "magnet:?xt=" + ("urn:btih:" if args.kind == "v1" else "urn:btmh:1220") + info_hash
    native_seed = None
    ports = []
    sessions = []
    if args.seed_engine == "mynou":
        import shutil
        native_root = output / "native-seed"
        destination = native_root / "data" / info_hash / "payload.bin"
        destination.parent.mkdir(parents=True)
        shutil.copyfile(payload, destination)
        seed_log = (output / "native-seed.log").open("wb")
        native_seed = await asyncio.create_subprocess_exec(
            "/usr/bin/time", "-l", "-o", str(output / "seed.time"),
            str(args.mynou), "seed", str(native_root), str(torrent), "", "0",
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
            stderr=seed_log, env=dict(os.environ, TMPDIR="/tmp"), start_new_session=True)
        try:
            seed_report = json.loads(await asyncio.wait_for(native_seed.stdout.readline(), 60))
        except BaseException:
            os.killpg(native_seed.pid, signal.SIGKILL)
            await native_seed.wait()
            seed_log.close()
            raise
        ports.append(seed_report["port"])
        (output / "seed-ready.json").write_text(json.dumps(seed_report, indent=2) + "\n")
    else:
        for _ in range(args.seeds):
            ses = session(args.peer_encryption)
            params = lt.add_torrent_params()
            params.ti, params.save_path = info, str(seed_root)
            params.flags &= ~lt.torrent_flags.auto_managed
            params.flags &= ~lt.torrent_flags.paused
            seed = ses.add_torrent(params)
            deadline = time.monotonic() + 30
            while not seed.status().is_seeding or not ses.listen_port():
                if time.monotonic() > deadline:
                    raise TimeoutError("seed preparation")
                await asyncio.sleep(0.01)
            ports.append(ses.listen_port())
            sessions.append(ses)
    environment = {
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "dirty": subprocess.check_output(["git", "status", "--porcelain"], text=True),
        "platform": platform.platform(), "python": sys.version,
        "libtorrent": lt.__version__, "mynou_sha256": digest(args.mynou),
        "rqbit_version": subprocess.check_output([str(args.rqbit), "--version"], text=True).strip() if args.rqbit else None,
        "rqbit_sha256": digest(args.rqbit) if args.rqbit else None,
        "payload_sha256": expected, "bytes": payload.stat().st_size,
        "piece_bytes": 1 << 20, "repeats": args.repeats, "kind": args.kind,
        "seed_engine": args.seed_engine, "seeds": len(ports),
        "peer_encryption": args.peer_encryption,
        "method": "trackerless synthetic magnet, common seeds, explicit loopback peers, discovery disabled; same delayed TCP proxy for every downloader; throughput uses process launch to exit; ready excludes shutdown, CPU/RSS include shutdown; no equal durability claim",
    }
    (output / "environment.json").write_text(json.dumps(environment, indent=2) + "\n")
    engines = ["mynou", "libtorrent"] + (["rqbit"] if args.rqbit else [])
    if args.engines:
        if any(engine not in engines for engine in args.engines):
            raise ValueError("unknown/unavailable downloader")
        engines = args.engines
    cases = [(rtt, rate) for rtt in args.rtt for rate in args.rates]
    results = []
    # Interleave and rotate engines to reduce cache/temperature ordering bias.
    for repeat in range(args.repeats):
        random.Random(repeat).shuffle(cases)
        order = engines[repeat % len(engines):] + engines[:repeat % len(engines)]
        for rtt, rate in cases:
            for engine in order:
                label = f"{engine}-rtt{rtt}-rate{rate}-run{repeat}"
                root = output / label
                root.mkdir()  # Refuse accidental reuse of partial downloads.
                proxies = [Proxy(port, rtt) for port in ports]
                servers = [await asyncio.start_server(proxy.accept, "127.0.0.1", 0) for proxy in proxies]
                peer = ",".join(f"127.0.0.1:{server.sockets[0].getsockname()[1]}" for server in servers)
                if engine == "mynou":
                    command = [str(args.mynou), "download", str(root), magnet, peer, str(rate)]
                    downloaded = root / "data" / info_hash / "payload.bin"
                elif engine == "libtorrent":
                    command = [sys.executable, __file__, "--lt-child", "--root", str(root),
                               "--source", magnet, "--peers", peer, "--rate", str(rate),
                               "--peer-encryption", args.peer_encryption]
                    downloaded = root / "payload.bin"
                else:
                    command = [str(args.rqbit), "--disable-dht", "--disable-dht-persistence",
                               "--disable-upnp-port-forward", "--disable-lsd", "--disable-trackers",
                               "--disable-tcp-listen", "-v", "error"]
                    if rate:
                        command += ["--ratelimit-download", str(rate)]
                    command += ["download", "--disable-http-api", "--exit-on-finish",
                                "--initial-peers", peer, "-o", str(root), magnet]
                    downloaded = root / "payload.bin"
                timing = output / (label + ".time")
                seed_before = [s.get_torrents()[0].status().total_payload_upload for s in sessions]
                started = time.monotonic()
                process = await asyncio.create_subprocess_exec(
                    "/usr/bin/time", "-l", "-o", str(timing), *command,
                    env=dict(os.environ, TMPDIR="/tmp", XDG_CONFIG_HOME=str(root / "config"),
                             XDG_CACHE_HOME=str(root / "cache"), XDG_DATA_HOME=str(root / "local")),
                    start_new_session=True,
                    stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
                try:
                    stdout, stderr = await asyncio.wait_for(process.communicate(), timeout=200)
                except BaseException:
                    os.killpg(process.pid, signal.SIGKILL)
                    await process.wait()
                    raise
                finally:
                    for server in servers:
                        server.close()
                        await server.wait_closed()
                    for proxy in proxies:
                        await proxy.close()
                elapsed = time.monotonic() - started
                (output / (label + ".log")).write_bytes(stdout + stderr)
                if process.returncode:
                    raise RuntimeError(f"{label} failed: {(stdout + stderr).decode(errors='replace')[-2000:]}")
                report = next((json.loads(line) for line in stdout.splitlines() if line.startswith(b'{')), {})
                ready = report.get("ready_s", elapsed)
                text = timing.read_text()
                cpu = re.search(r"([0-9.]+) real\s+([0-9.]+) user\s+([0-9.]+) sys", text)
                rss = re.search(r"(\d+)\s+maximum resident set size", text)
                if not cpu or not rss:
                    raise ValueError("requires macOS /usr/bin/time -l")
                if digest(downloaded) != expected:
                    raise ValueError(f"corrupt payload in {label}")
                seed_bytes = [s.get_torrents()[0].status().total_payload_upload - before
                              for s, before in zip(sessions, seed_before)]
                if args.seeds > 1 and not all(value > 0 for value in seed_bytes):
                    raise ValueError(f"not every seed contributed payload in {label}: {seed_bytes}")
                row = dict(engine=engine, rtt_ms=rtt, rate_bps=rate, repeat=repeat,
                           ready_s=ready, wall_s=elapsed, mib_s=args.mib / elapsed,
                           cpu_s=float(cpu[2]) + float(cpu[3]), rss_bytes=int(rss[1]), verified=True)
                row["seed_payload_bytes"] = seed_bytes
                row["handshakes"] = {key: sum(proxy.handshakes[key] for proxy in proxies)
                                     for key in ["plaintext", "other"]}
                if args.peer_encryption == "disabled" and row["handshakes"]["other"]:
                    raise ValueError(f"unexpected non-plaintext handshake in {label}")
                if "open_s" in report:
                    row["phases"] = {key: report[key] for key in ["open_s", "admission_s", "work_s", "shutdown_s"]}
                results.append(row)
                (output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
                print(json.dumps(row), flush=True)
                # Keep raw measurements and logs, release large payloads.
                downloaded.unlink()
    if native_seed:
        native_seed.stdin.write(b"stop\n")
        await native_seed.stdin.drain()
        await asyncio.wait_for(native_seed.wait(), 15)
        seed_log.close()
        if native_seed.returncode:
            raise RuntimeError("native seed failed during shutdown")
        timing = (output / "seed.time").read_text()
        cpu = re.search(r"([0-9.]+) real\s+([0-9.]+) user\s+([0-9.]+) sys", timing)
        rss = re.search(r"(\d+)\s+maximum resident set size", timing)
        if not cpu or not rss:
            raise ValueError("missing native seed resource measurements")
        (output / "seed-resources.json").write_text(json.dumps({
            "wall_s": float(cpu[1]), "cpu_s": float(cpu[2]) + float(cpu[3]),
            "rss_bytes": int(rss[1]), "includes": "native seed startup, rehash, all consumers and shutdown; excludes proxy and downloaders",
        }, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mynou", type=Path)
    parser.add_argument("--rqbit", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--mib", type=int, default=32)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--rtt", type=int, nargs="+", default=[0, 50, 100])
    parser.add_argument("--rates", type=int, nargs="+", default=[0, 16 << 20])
    parser.add_argument("--seed-engine", choices=["mynou", "libtorrent"], default="libtorrent")
    parser.add_argument("--peer-encryption", choices=["negotiated", "disabled", "forced"], default="negotiated",
                        help="libtorrent MSE negotiation (default), common plaintext protocol, or required RC4")
    parser.add_argument("--engines", choices=["mynou", "libtorrent", "rqbit"], nargs="+")
    parser.add_argument("--kind", choices=["v1", "v2", "hybrid"], default="v1")
    parser.add_argument("--seeds", type=int, default=1)
    parser.add_argument("--lt-child", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--root", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--source", help=argparse.SUPPRESS)
    parser.add_argument("--peers", help=argparse.SUPPRESS)
    parser.add_argument("--rate", type=int, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.lt_child:
        lt_download(args)
    else:
        if not args.mynou or not args.output or min(args.mib, args.repeats, args.seeds) <= 0:
            parser.error("--mynou and --output required; positive --mib and --repeats")
        if args.seed_engine == "mynou" and args.seeds != 1:
            parser.error("native seed mode currently supports one seed")
        if min(args.rtt + args.rates) < 0:
            parser.error("RTT and rates must be nonnegative")
        if (platform.system(), platform.machine()) != ("Darwin", "arm64"):
            parser.error("this runner requires native macOS arm64 and BSD time")
        asyncio.run(run(args))


if __name__ == "__main__":
    main()
