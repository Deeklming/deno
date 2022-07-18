// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.
"use strict";

((window) => {
  const core = window.Deno.core;
  const { pathFromURL } = window.__bootstrap.util;
  const { illegalConstructorKey } = window.__bootstrap.webUtil;
  const { add, remove } = window.__bootstrap.abortSignal;
  const {
    ArrayPrototypeMap,
    ObjectEntries,
    String,
    TypeError,
    Uint8Array,
    PromiseAll,
  } = window.__bootstrap.primordials;
  const { readableStreamForRid, writableStreamForRid } =
    window.__bootstrap.streamUtils;

  function spawnChild(command, {
    args = [],
    cwd = undefined,
    clearEnv = false,
    env = {},
    uid = undefined,
    gid = undefined,
    stdin = "null",
    stdout = "piped",
    stderr = "piped",
    signal = undefined,
  } = {}) {
    const child = core.opSync("op_spawn_child", {
      cmd: pathFromURL(command),
      args: ArrayPrototypeMap(args, String),
      cwd: pathFromURL(cwd),
      clearEnv,
      env: ObjectEntries(env),
      uid,
      gid,
      stdin,
      stdout,
      stderr,
    });
    return new Child(illegalConstructorKey, {
      ...child,
      signal,
    });
  }

  async function collectOutput(readableStream) {
    if (!(readableStream instanceof ReadableStream)) {
      return null;
    }

    const bufs = [];
    let size = 0;
    for await (const chunk of readableStream) {
      bufs.push(chunk);
      size += chunk.byteLength;
    }

    const buffer = new Uint8Array(size);
    let offset = 0;
    for (const chunk of bufs) {
      buffer.set(chunk, offset);
      offset += chunk.byteLength;
    }

    return buffer;
  }

  class Child {
    #rid;

    #pid;
    get pid() {
      return this.#pid;
    }

    #stdin = null;
    get stdin() {
      if (this.#stdin == null) {
        throw new TypeError("stdin is not piped");
      }
      return this.#stdin;
    }

    #stdout = null;
    get stdout() {
      if (this.#stdout == null) {
        throw new TypeError("stdout is not piped");
      }
      return this.#stdout;
    }

    #stderr = null;
    get stderr() {
      if (this.#stderr == null) {
        throw new TypeError("stderr is not piped");
      }
      return this.#stderr;
    }

    constructor(key = null, {
      signal,
      rid,
      pid,
      stdinRid,
      stdoutRid,
      stderrRid,
    } = null) {
      if (key !== illegalConstructorKey) {
        throw new TypeError("Illegal constructor.");
      }

      this.#rid = rid;
      this.#pid = pid;

      if (stdinRid !== null) {
        this.#stdin = writableStreamForRid(stdinRid);
      }

      if (stdoutRid !== null) {
        this.#stdout = readableStreamForRid(stdoutRid);
      }

      if (stderrRid !== null) {
        this.#stderr = readableStreamForRid(stderrRid);
      }

      const onAbort = () => this.kill("SIGTERM");
      signal?.[add](onAbort);

      this.#status = core.opAsync("op_spawn_wait", this.#rid).then((res) => {
        this.#rid = null;
        signal?.[remove](onAbort);
        return res;
      });
    }

    #status;
    get status() {
      return this.#status;
    }

    async output() {
      if (this.#stdout?.locked) {
        throw new TypeError(
          "Can't collect output because stdout is locked",
        );
      }
      if (this.#stderr?.locked) {
        throw new TypeError(
          "Can't collect output because stderr is locked",
        );
      }

      const [status, stdout, stderr] = await PromiseAll([
        this.#status,
        collectOutput(this.#stdout),
        collectOutput(this.#stderr),
      ]);

      return {
        success: status.success,
        code: status.code,
        signal: status.signal,
        get stdout() {
          if (stdout == null) {
            throw new TypeError("stdout is not piped");
          }
          return stdout;
        },
        get stderr() {
          if (stderr == null) {
            throw new TypeError("stderr is not piped");
          }
          return stderr;
        },
      };
    }

    kill(signo = "SIGTERM") {
      if (this.#rid === null) {
        throw new TypeError("Child process has already terminated.");
      }
      core.opSync("op_kill", this.#pid, signo);
    }
  }

  function spawn(command, options) {
    if (options?.stdin === "piped") {
      throw new TypeError(
        "Piped stdin is not supported for this function, use 'Deno.spawnChild()' instead",
      );
    }
    return spawnChild(command, options).output();
  }

  function spawnSync(command, {
    args = [],
    cwd = undefined,
    clearEnv = false,
    env = {},
    uid = undefined,
    gid = undefined,
    stdin = "null",
    stdout = "piped",
    stderr = "piped",
  } = {}) {
    if (stdin === "piped") {
      throw new TypeError(
        "Piped stdin is not supported for this function, use 'Deno.spawnChild()' instead",
      );
    }
    const result = core.opSync("op_spawn_sync", {
      cmd: pathFromURL(command),
      args: ArrayPrototypeMap(args, String),
      cwd: pathFromURL(cwd),
      clearEnv,
      env: ObjectEntries(env),
      uid,
      gid,
      stdin,
      stdout,
      stderr,
    });
    return {
      success: result.status.success,
      code: result.status.code,
      signal: result.status.signal,
      get stdout() {
        if (result.stdout == null) {
          throw new TypeError("stdout is not piped");
        }
        return result.stdout;
      },
      get stderr() {
        if (result.stderr == null) {
          throw new TypeError("stderr is not piped");
        }
        return result.stderr;
      },
    };
  }

  window.__bootstrap.spawn = {
    Child,
    spawnChild,
    spawn,
    spawnSync,
  };
})(this);
