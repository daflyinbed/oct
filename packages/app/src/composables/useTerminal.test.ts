import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createTerminalConnection } from "./useTerminal";

vi.mock("@/api/client", () => ({ default: {} }));

class FakeSocket extends EventTarget {
  static OPEN = 1;
  static instances: FakeSocket[] = [];
  readyState = 1;
  binaryType = "";
  bufferedAmount = 0;
  url: URL;
  sent: (string | Uint8Array)[] = [];
  constructor(url: URL) {
    super();
    this.url = url;
    FakeSocket.instances.push(this);
  }
  send(data: string | Uint8Array) {
    this.sent.push(data);
  }
  close() {
    this.readyState = 3;
    this.dispatchEvent(new Event("close"));
  }
  control(value: unknown) {
    this.dispatchEvent(
      new MessageEvent("message", { data: JSON.stringify(value) }),
    );
  }
  output(offset: number, bytes: Uint8Array) {
    const frame = new Uint8Array(8 + bytes.length);
    new DataView(frame.buffer).setBigUint64(0, BigInt(offset));
    frame.set(bytes, 8);
    this.dispatchEvent(new MessageEvent("message", { data: frame.buffer }));
  }
}

beforeEach(() => {
  vi.useFakeTimers();
  FakeSocket.instances = [];
  vi.stubGlobal("WebSocket", FakeSocket);
  vi.stubGlobal("window", { location: new URL("https://oct.example/") });
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

function setup() {
  const sink = { write: vi.fn(), reset: vi.fn(), resize: vi.fn() };
  const connection = createTerminalConnection("project", "session", sink);
  connection.connect();
  const socket = FakeSocket.instances[0]!;
  socket.control({
    type: "ready",
    offset: 0,
    cols: 80,
    rows: 24,
    truncated: false,
    replay_reply_after: 0,
  });
  return { connection, socket, sink };
}

describe("terminal transport", () => {
  it("answers first-attachment shell queries during replay while holding user input", () => {
    const { connection, socket, sink } = setup();
    socket.control({
      type: "ready",
      offset: 0,
      cols: 80,
      rows: 24,
      truncated: false,
      replay_reply_after: 0,
    });
    sink.write.mockImplementation(() => connection.write("\u001B[?62c"));
    connection.write("user input before live");
    expect(socket.sent).toEqual([]);
    socket.output(0, new TextEncoder().encode("\u001B[0c"));
    expect(socket.sent).toEqual([
      new TextEncoder().encode("\u001B[?62c"),
      JSON.stringify({ type: "ack", offset: 4 }),
    ]);
    connection.dispose();
  });

  it("answers new queries buffered while an existing session was disconnected", () => {
    const { connection, socket, sink } = setup();
    const initial = new TextEncoder().encode("shell ready\r\n");
    socket.output(0, initial);
    socket.control({ type: "live" });
    connection.write("run program\r");
    socket.close();
    vi.advanceTimersByTime(500);
    const resumed = FakeSocket.instances[1]!;
    const after = initial.byteLength;
    resumed.control({
      type: "ready",
      offset: after,
      cols: 80,
      rows: 24,
      truncated: false,
      replay_reply_after: after,
    });
    sink.write.mockImplementation(() => connection.write("\u001B[0n"));
    connection.write("user input during replay");
    expect(resumed.sent).toEqual([]);
    const query = new TextEncoder().encode("\u001B[5n");
    resumed.output(after, query);
    expect(resumed.sent).toEqual([
      new TextEncoder().encode("\u001B[0n"),
      JSON.stringify({ type: "ack", offset: after + query.byteLength }),
    ]);
    connection.dispose();
  });

  it("suppresses processed queries after refresh and replies to an unread query in the same frame", () => {
    const { connection, socket, sink } = setup();
    const oldQuery = new TextEncoder().encode("\u001B[0c");
    const newQuery = new TextEncoder().encode("\u001B[5n");
    socket.control({
      type: "ready",
      offset: 0,
      cols: 80,
      rows: 24,
      truncated: false,
      replay_reply_after: oldQuery.byteLength,
    });
    sink.write.mockImplementation(() => connection.write("query reply"));
    const bytes = new Uint8Array([...oldQuery, ...newQuery]);
    socket.output(0, bytes);
    expect(sink.write.mock.calls.map(([part]) => Array.from(part))).toEqual([
      Array.from(oldQuery),
      Array.from(newQuery),
    ]);
    expect(socket.sent).toEqual([
      new TextEncoder().encode("query reply"),
      JSON.stringify({ type: "ack", offset: bytes.byteLength }),
    ]);
    // Only the unread half generates a reply, before acknowledging the frame.
    connection.dispose();
  });
  it("preserves binary UTF-8 fragments and resumes after the last received byte", () => {
    const { connection, socket, sink } = setup();
    connection.write("never send input during replay");
    expect(socket.sent).toEqual([]);
    const bytes = new TextEncoder().encode("你好\r\n");
    socket.output(0, bytes.slice(0, 1));
    socket.output(1, bytes.slice(1));
    expect(sink.write.mock.calls.map(([chunk]) => Array.from(chunk))).toEqual([
      Array.from(bytes.slice(0, 1)),
      Array.from(bytes.slice(1)),
    ]);
    socket.control({ type: "live" });
    connection.write("ssh target\r");
    expect(socket.sent.at(-1)).toEqual(
      new TextEncoder().encode("ssh target\r"),
    );
    connection.reconnect();
    const resumed = FakeSocket.instances[1]!;
    expect(resumed.url.protocol).toBe("wss:");
    expect(resumed.url.searchParams.get("after")).toBe(String(bytes.length));
    expect(sink.reset).not.toHaveBeenCalled();
    expect(resumed.sent).toEqual([]); // user input is never retried
    connection.dispose();
  });

  it("fits live PTYs and bounds resize and paste frames", () => {
    const { connection, socket } = setup();
    connection.resize(999, 999);
    socket.control({ type: "live" });
    expect(JSON.parse(socket.sent[0] as string)).toEqual({
      type: "resize",
      cols: 500,
      rows: 300,
    });
    const paste = "中".repeat(20_000);
    connection.write(paste);
    const chunks = socket.sent.filter(
      (item): item is Uint8Array => item instanceof Uint8Array,
    );
    expect(chunks.every((chunk) => chunk.length <= 16 * 1024)).toBe(true);
    expect(
      new TextDecoder().decode(
        new Uint8Array(chunks.flatMap((chunk) => Array.from(chunk))),
      ),
    ).toBe(paste);
    connection.dispose();
  });

  it("retries disconnected sockets, ignores stale events and stops retrying on dispose", () => {
    const { connection, socket } = setup();
    socket.control({ type: "live" });
    socket.close();
    expect(connection.status.value).toBe("disconnected");
    vi.advanceTimersByTime(500);
    expect(FakeSocket.instances).toHaveLength(2);
    socket.control({ type: "exit", exit_code: 9 });
    expect(connection.status.value).toBe("connecting");
    connection.dispose();
    vi.advanceTimersByTime(60_000);
    expect(FakeSocket.instances).toHaveLength(2);
  });

  it("shows replay gaps and distinguishes shell exit from a reconnectable disconnect", () => {
    const { connection, socket, sink } = setup();
    socket.control({
      type: "ready",
      offset: 2048,
      cols: 100,
      rows: 30,
      truncated: true,
      replay_reply_after: 2048,
    });
    expect(sink.reset).toHaveBeenCalledOnce();
    expect(connection.notice.value).toContain("缓存");
    socket.output(2048, new Uint8Array([65]));
    socket.control({ type: "exit", exit_code: 7 });
    socket.close();
    vi.advanceTimersByTime(60_000);
    expect(connection.status.value).toBe("exited");
    expect(connection.exitCode.value).toBe(7);
    expect(FakeSocket.instances).toHaveLength(1);
    connection.dispose();
  });

  it("surfaces protocol errors without silently skipping output", () => {
    const { connection, socket, sink } = setup();
    socket.output(99, new Uint8Array([65]));
    expect(connection.status.value).toBe("error");
    expect(connection.error.value).toContain("顺序");
    expect(sink.write).not.toHaveBeenCalled();
    vi.advanceTimersByTime(60_000);
    expect(FakeSocket.instances).toHaveLength(1);
    connection.dispose();
  });

  it("does not acknowledge or advance past output rejected by the renderer", () => {
    const { connection, socket, sink } = setup();
    const bytes = new TextEncoder().encode("\u001B[0c");
    sink.write.mockImplementationOnce(() => {
      throw new Error("renderer unavailable");
    });
    socket.output(0, bytes);
    expect(connection.status.value).toBe("error");
    expect(connection.error.value).toBe("renderer unavailable");
    expect(socket.sent).toEqual([]);

    connection.reconnect();
    const resumed = FakeSocket.instances[1]!;
    expect(resumed.url.searchParams.get("after")).toBe("0");
    resumed.control({
      type: "ready",
      offset: 0,
      cols: 80,
      rows: 24,
      truncated: false,
      replay_reply_after: 0,
    });
    sink.write.mockImplementation(() => connection.write("\u001B[?62c"));
    resumed.output(0, bytes);
    expect(resumed.sent).toEqual([
      new TextEncoder().encode("\u001B[?62c"),
      JSON.stringify({ type: "ack", offset: bytes.byteLength }),
    ]);
    connection.dispose();
  });
});
