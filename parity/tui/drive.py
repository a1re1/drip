"""pty driver rendering through a real terminal emulator (pyte).
usage: drive2.py <bin> <home> <cwd> <steps...>   steps: text (unicode-escaped), @sleep, @snap <label>
Prints the visible screen (30x100) at every @snap, so two harnesses can be diffed
on what a human would actually see."""
import os, pty, re, sys, time, select, fcntl, termios, struct, pyte
bin_, home, cwd = sys.argv[1:4]; steps = sys.argv[4:]
COLS, ROWS = 100, 30
pid, fd = pty.fork()
if pid == 0:
    os.environ["DRIP_HOME"] = home; os.environ["LCI_HOME"] = home
    os.environ["TERM"] = "xterm-256color"; os.environ["COLUMNS"] = str(COLS); os.environ["LINES"] = str(ROWS)
    os.chdir(cwd)
    # TUI_FLAG="" drives a program that is a TUI by itself (lciw / dripw).
    flag = [os.environ["TUI_FLAG"]] if os.environ.get("TUI_FLAG", "--tui") else []
    os.execvp(bin_, [bin_] + flag + os.environ.get("DRIP_ARGS", "").split())
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
ESCAPES = {"r": "\r", "n": "\n", "t": "\t", "e": "\x1b", "\\": "\\"}
def keys(step):
    """`\\r`, `\\n`, `\\t`, `\\e`, `\\xHH`, `\\uHHHH` escapes; everything else (incl. non-Latin text) is sent as UTF-8."""
    def sub(m):
        body = m.group(1)
        if body[0] in "xu": return chr(int(body[1:], 16))
        return ESCAPES[body]
    return re.sub(r"\\(x[0-9a-fA-F]{2}|u[0-9a-fA-F]{4}|[rnte\\])", sub, step).encode("utf-8")
def send(data):
    try: os.write(fd, data)
    except OSError:
        print("===== child exited before the script finished"); return False
    return True
screen = pyte.HistoryScreen(COLS, ROWS, history=2000); stream = pyte.ByteStream(screen)
def drain(t=0.6):
    end = time.time() + t
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            try: stream.feed(os.read(fd, 65536))
            except OSError: return
def snap(label):
    print(f"===== {label}")
    for line in screen.display:
        print(line.rstrip())
drain(1.5)
for step in steps:
    if step == "@sleep": drain(1.0); continue
    if step.startswith("@snap"): snap(step[6:].strip() or "snap"); continue
    if step.startswith("@resize"):
        rows, cols = step.split()[1].split("x"); rows, cols = int(rows), int(cols)
        screen.resize(rows, cols); fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0)); drain(0.8); continue
    if step.startswith("@paste "):
        if not send(b"\x1b[200~" + keys(step[7:]) + b"\x1b[201~"): break
        drain(0.8); continue
    if not send(keys(step)): break
    drain(0.8)
send(b"\x03"); drain(1.0)
try: os.waitpid(pid, 0)
except Exception: pass
