"""Template-server client: hand a criu restore command line (and this process's stdio, cgroup and mount namespace) to the
pageshare template server and exit with the restore's result.  Usage: pageshare_client.py SOCKET criu restore ARGS...

A server that is not there (no socket file, or nobody listening: ECONNREFUSED) fails at once with "no template server": the
caller checked the server just before (tmplsrv.require), so waiting would only hide a dead server for minutes.  Only a full
listen backlog (EAGAIN, many slots restoring at once) is waited for, up to 120 s."""
import array, errno, socket, struct, sys, time
sock, argv = sys.argv[1], sys.argv[2:]
s = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
t0 = time.monotonic()
while True:
    try:
        s.connect(sock); break
    except OSError as e:
        if e.errno == errno.EAGAIN and time.monotonic() - t0 < 120:
            time.sleep(0.05); continue
        sys.exit(f"pageshare_client: no template server at {sock}: {e.strerror or e}")
s.sendmsg([b"\0".join(a.encode() for a in argv) + b"\0"], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", [0, 1, 2]))])
d = s.recv(4)
if len(d) != 4:
    sys.exit("pageshare_client: the server closed the connection without a result")
sys.exit(0 if struct.unpack("i", d)[0] == 0 else 1)
