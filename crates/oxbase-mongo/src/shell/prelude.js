// The mongo-shell subset of oxbase (crates/oxbase-mongo/src/shell/mod.rs documents it).
// Loaded once per session runtime, before any statement. Everything a script can reach of the
// host goes through `host(op, payload)` (src/shell/host.rs); this file builds the shell API
// on top of it: helper types, Extended JSON, `db`, collections, cursors, and the description
// of a statement's value for Rust (`__oxbase_describe`).
(function (host) {
  'use strict';
  const G = globalThis;
  delete G.__oxbase_host;

  // Deep enough that a statement's own frame is in the stack of errors thrown by the API.
  Error.stackTraceLimit = 64;

  const EJ = Symbol('ejson');
  const MAX_DEPTH = 100;
  const INT32_MIN = -2147483648;
  const INT32_MAX = 2147483647;

  // ---- errors -------------------------------------------------------------------------------

  class MongoServerError extends Error {
    constructor(message, codeName, code) {
      super(message);
      this.name = 'MongoServerError';
      if (codeName !== undefined && codeName !== null) this.codeName = codeName;
      if (code !== undefined) this.code = code;
    }
  }

  function hostError(e) {
    const err = new MongoServerError(e.message, e.codeName);
    err.name = e.name;
    Object.defineProperty(err, 'oxbaseErrorId', { value: e.id, enumerable: false });
    return err;
  }

  function call(op, payload) {
    const reply = JSON.parse(host(op, JSON.stringify(payload === undefined ? null : payload)));
    if (reply.error) throw hostError(reply.error);
    return reply.ok;
  }

  function fail(message) {
    throw new TypeError(message);
  }

  // ---- helper types -------------------------------------------------------------------------

  function define(proto, methods) {
    for (const key of Reflect.ownKeys(methods)) {
      Object.defineProperty(proto, key, {
        value: methods[key],
        writable: true,
        configurable: true,
        enumerable: false,
      });
    }
  }

  function ObjectId(hex) {
    if (!new.target) return new ObjectId(hex);
    if (hex instanceof ObjectId) hex = hex.$oid;
    if (hex === undefined || hex === null) hex = call('oid');
    if (typeof hex !== 'string' || !/^[0-9a-fA-F]{24}$/.test(hex)) {
      fail('ObjectId() takes 24 hexadecimal digits');
    }
    Object.defineProperty(this, '$oid', { value: hex.toLowerCase(), enumerable: true });
  }
  define(ObjectId.prototype, {
    [EJ]() { return { $oid: this.$oid }; },
    toString() { return this.$oid; },
    toHexString() { return this.$oid; },
    toJSON() { return this.$oid; },
    equals(other) { return other instanceof ObjectId && other.$oid === this.$oid; },
    getTimestamp() { return new Date(parseInt(this.$oid.slice(0, 8), 16) * 1000); },
    inspect() { return 'ObjectId("' + this.$oid + '")'; },
  });

  // Compares two decimal integer texts without signs (same length rules as numbers).
  function inInt64(text) {
    const negative = text[0] === '-';
    const digits = text.replace(/^[+-]/, '').replace(/^0+(?=\d)/, '');
    const limit = negative ? '9223372036854775808' : '9223372036854775807';
    return digits.length < limit.length || (digits.length === limit.length && digits <= limit);
  }

  function Long(value) {
    if (!new.target) return new Long(value);
    let text;
    if (value instanceof Long) text = value.value;
    else if (typeof value === 'number') {
      if (!Number.isSafeInteger(value)) {
        fail('NumberLong() takes a whole number up to 2^53, or a decimal string for larger ones');
      }
      text = String(value);
    } else if (typeof value === 'string') text = value.trim();
    else if (value === undefined) text = '0';
    else fail('NumberLong() takes a number or a decimal string');
    if (!/^[+-]?\d+$/.test(text) || !inInt64(text)) {
      fail('NumberLong() takes a 64-bit integer, not ' + JSON.stringify(String(value)));
    }
    text = text.replace(/^\+/, '').replace(/^(-?)0+(?=\d)/, '$1');
    if (text === '-0') text = '0';
    Object.defineProperty(this, 'value', { value: text, enumerable: false });
  }
  define(Long.prototype, {
    [EJ]() { return { $numberLong: this.value }; },
    valueOf() { return Number(this.value); },
    toNumber() { return Number(this.value); },
    toString() { return this.value; },
    toJSON() { return this.relaxed(); },
    relaxed() {
      const n = Number(this.value);
      return Number.isSafeInteger(n) ? n : { $numberLong: this.value };
    },
    equals(other) { return other instanceof Long && other.value === this.value; },
  });

  function Int32(value) {
    if (!new.target) return new Int32(value);
    const n = Number(value === undefined ? 0 : value);
    if (!Number.isInteger(n) || n < INT32_MIN || n > INT32_MAX) {
      fail('NumberInt() takes a 32-bit integer');
    }
    Object.defineProperty(this, 'value', { value: n, enumerable: false });
  }
  define(Int32.prototype, {
    [EJ]() { return { $numberInt: String(this.value) }; },
    valueOf() { return this.value; },
    toString() { return String(this.value); },
    toJSON() { return this.value; },
  });

  function Decimal128(text) {
    if (!new.target) return new Decimal128(text);
    if (text instanceof Decimal128) text = text.value;
    if (typeof text === 'number') text = String(text);
    if (typeof text !== 'string') fail('NumberDecimal() takes a decimal string');
    text = text.trim();
    if (!/^[+-]?((\d+(\.\d*)?|\.\d+)([eE][+-]?\d+)?|Inf(inity)?|NaN)$/i.test(text)) {
      fail('NumberDecimal() takes a decimal number, not ' + JSON.stringify(text));
    }
    Object.defineProperty(this, 'value', { value: text, enumerable: false });
  }
  define(Decimal128.prototype, {
    [EJ]() { return { $numberDecimal: this.value }; },
    toString() { return this.value; },
    toJSON() { return { $numberDecimal: this.value }; },
  });

  function Timestamp(t, i) {
    if (!new.target) return new Timestamp(t, i);
    if (t !== null && typeof t === 'object') {
      i = t.i;
      t = t.t;
    }
    t = t === undefined ? 0 : Number(t);
    i = i === undefined ? 0 : Number(i);
    if (![t, i].every((n) => Number.isInteger(n) && n >= 0 && n <= 4294967295)) {
      fail('Timestamp() takes two unsigned 32-bit integers');
    }
    Object.defineProperty(this, 't', { value: t, enumerable: true });
    Object.defineProperty(this, 'i', { value: i, enumerable: true });
  }
  define(Timestamp.prototype, {
    [EJ]() { return { $timestamp: { t: this.t, i: this.i } }; },
    toJSON() { return { $timestamp: { t: this.t, i: this.i } }; },
  });

  const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  function hexToBase64(hex) {
    let out = '';
    for (let i = 0; i < hex.length; i += 6) {
      const chunk = hex.slice(i, i + 6);
      const bytes = chunk.length / 2;
      const n = parseInt(chunk.padEnd(6, '0'), 16);
      const chars = [18, 12, 6, 0].map((s) => B64[(n >> s) & 63]);
      out += chars.slice(0, bytes + 1).join('') + '='.repeat(3 - bytes);
    }
    return out;
  }
  function base64ToHex(text) {
    let bits = 0;
    let value = 0;
    let out = '';
    for (const c of text.replace(/=+$/, '')) {
      const v = B64.indexOf(c);
      if (v < 0) fail('invalid base64 data');
      value = (value << 6) | v;
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        out += ((value >> bits) & 255).toString(16).padStart(2, '0');
      }
    }
    return out;
  }

  function Binary(base64, subType) {
    if (!new.target) return new Binary(base64, subType);
    if (typeof base64 !== 'string' || !/^[A-Za-z0-9+/]*={0,2}$/.test(base64) || base64.length % 4 !== 0) {
      fail('BinData() takes base64 data');
    }
    const sub = typeof subType === 'string' ? parseInt(subType, 16) : Number(subType);
    if (!Number.isInteger(sub) || sub < 0 || sub > 255) fail('BinData() takes a subtype 0-255');
    Object.defineProperty(this, 'base64', { value: base64, enumerable: false });
    Object.defineProperty(this, 'subType', { value: sub, enumerable: false });
  }
  define(Binary.prototype, {
    [EJ]() {
      return { $binary: { base64: this.base64, subType: this.subType.toString(16).padStart(2, '0') } };
    },
    hex() { return base64ToHex(this.base64); },
    toJSON() { return this[EJ](); },
    toString() { return this.subType === 4 ? uuidText(this.hex()) : this.base64; },
  });
  function BinData(subType, base64) {
    return new Binary(base64, subType);
  }
  function uuidText(hex) {
    return [hex.slice(0, 8), hex.slice(8, 12), hex.slice(12, 16), hex.slice(16, 20), hex.slice(20)].join('-');
  }
  function UUID(text) {
    let hex = text === undefined ? call('uuid') : String(text).replace(/-/g, '');
    if (!/^[0-9a-fA-F]{32}$/.test(hex)) fail('UUID() takes 32 hexadecimal digits');
    return new Binary(hexToBase64(hex.toLowerCase()), 4);
  }

  function MinKey() {
    if (!new.target) return new MinKey();
  }
  define(MinKey.prototype, { [EJ]() { return { $minKey: 1 }; }, toJSON() { return { $minKey: 1 }; } });
  function MaxKey() {
    if (!new.target) return new MaxKey();
  }
  define(MaxKey.prototype, { [EJ]() { return { $maxKey: 1 }; }, toJSON() { return { $maxKey: 1 }; } });

  // Values the shell keeps as they came from the server (code, DB pointers, regular
  // expressions JavaScript cannot hold).
  function Raw(ejson) {
    Object.defineProperty(this, 'ejson', { value: ejson, enumerable: false });
  }
  define(Raw.prototype, { [EJ]() { return this.ejson; }, toJSON() { return this.ejson; } });

  function ISODate(text) {
    const d = text === undefined ? new Date() : new Date(text);
    if (Number.isNaN(d.getTime())) fail('ISODate() takes an ISO-8601 date, not ' + JSON.stringify(String(text)));
    return d;
  }

  // ---- Extended JSON ------------------------------------------------------------------------

  function isHelper(v) {
    return v !== null && typeof v === 'object' && typeof v[EJ] === 'function';
  }

  function isDocument(v) {
    if (v === null || typeof v !== 'object' || Array.isArray(v) || isHelper(v)) return false;
    const proto = Object.getPrototypeOf(v);
    return proto === Object.prototype || proto === null;
  }

  function regexOptions(flags) {
    return flags.split('').filter((f) => 'imsux'.includes(f)).sort().join('');
  }

  function numberEJSON(n) {
    if (Number.isInteger(n) && n >= INT32_MIN && n <= INT32_MAX && !Object.is(n, -0)) {
      return { $numberInt: String(n) };
    }
    if (Object.is(n, -0)) return { $numberDouble: '-0.0' };
    if (Number.isNaN(n)) return { $numberDouble: 'NaN' };
    if (!Number.isFinite(n)) return { $numberDouble: n > 0 ? 'Infinity' : '-Infinity' };
    let text = String(n);
    if (Number.isInteger(n) && !/[e.]/.test(text)) text += '.0';
    return { $numberDouble: text };
  }

  function dateMillis(d) {
    const ms = d.getTime();
    if (Number.isNaN(ms)) throw new RangeError('an invalid date cannot be stored');
    return ms;
  }

  // A JavaScript value as canonical Extended JSON (a JSON-ready value).
  function toEJSON(v, depth, seen) {
    depth = depth || 0;
    seen = seen || new Set();
    if (depth > MAX_DEPTH) throw new RangeError('the value is nested more than 100 levels deep');
    switch (typeof v) {
      case 'undefined': return undefined;
      case 'boolean':
      case 'string': return v;
      case 'number': return numberEJSON(v);
      case 'bigint': return { $numberLong: Long(v.toString()).value };
      case 'function': throw new TypeError('a function cannot be stored in a document');
      case 'symbol': throw new TypeError('a symbol cannot be stored in a document');
      default: break;
    }
    if (v === null) return null;
    if (isHelper(v)) return v[EJ]();
    if (v instanceof Date) return { $date: { $numberLong: String(dateMillis(v)) } };
    if (v instanceof RegExp) return { $regularExpression: { pattern: v.source, options: regexOptions(v.flags) } };
    if (v instanceof Number || v instanceof String || v instanceof Boolean) return toEJSON(v.valueOf(), depth, seen);
    if (v instanceof Database) return v.getName();
    if (v instanceof Collection) return v.getFullName();
    if (v instanceof Cursor) throw new TypeError('a cursor cannot be stored: call toArray() first');
    if (seen.has(v)) throw new TypeError('the value contains a cycle');
    seen.add(v);
    let out;
    if (Array.isArray(v)) {
      out = v.map((x) => {
        const e = toEJSON(x, depth + 1, seen);
        return e === undefined ? null : e;
      });
    } else {
      out = {};
      for (const key of Object.keys(v)) {
        const e = toEJSON(v[key], depth + 1, seen);
        if (e !== undefined) out[key] = e;
      }
    }
    seen.delete(v);
    return out;
  }

  // A JavaScript value as relaxed Extended JSON (for printing).
  function toRelaxed(v, depth, seen) {
    depth = depth || 0;
    seen = seen || new Set();
    if (depth > MAX_DEPTH) throw new RangeError('the value is nested more than 100 levels deep');
    if (typeof v === 'number') return Number.isFinite(v) ? v : numberEJSON(v);
    if (typeof v === 'bigint') return v.toString();
    if (typeof v === 'function') return '[function ' + (v.name || 'anonymous') + ']';
    if (typeof v === 'symbol') return v.toString();
    if (v === null || typeof v !== 'object') return v;
    if (v instanceof Long) return v.relaxed();
    if (v instanceof Int32) return v.value;
    if (isHelper(v)) return v[EJ]();
    if (v instanceof Date) {
      const ms = v.getTime();
      if (Number.isNaN(ms)) return 'Invalid Date';
      const year = v.getUTCFullYear();
      return year >= 1970 && year <= 9999 ? { $date: v.toISOString() } : { $date: { $numberLong: String(ms) } };
    }
    if (v instanceof RegExp) return { $regularExpression: { pattern: v.source, options: regexOptions(v.flags) } };
    if (v instanceof Database) return v.getName();
    if (v instanceof Collection) return v.getFullName();
    if (v instanceof Cursor) return '[cursor]';
    if (seen.has(v)) return '[cycle]';
    seen.add(v);
    let out;
    if (Array.isArray(v)) out = v.map((x) => { const r = toRelaxed(x, depth + 1, seen); return r === undefined ? null : r; });
    else {
      out = {};
      for (const key of Object.keys(v)) {
        const r = toRelaxed(v[key], depth + 1, seen);
        if (r !== undefined) out[key] = r;
      }
    }
    seen.delete(v);
    return out;
  }

  // Extended JSON (canonical or relaxed) as JavaScript values with the helper types.
  function revive(v) {
    if (Array.isArray(v)) return v.map(revive);
    if (v === null || typeof v !== 'object') return v;
    const keys = Object.keys(v);
    if (keys.length > 0 && keys[0][0] === '$') {
      const first = keys[0];
      const x = v[first];
      switch (first) {
        case '$oid': return new ObjectId(x);
        case '$numberInt': return Number(x);
        case '$numberLong': return new Long(x);
        case '$numberDouble': return Number(x);
        case '$numberDecimal': return new Decimal128(x);
        case '$date': {
          let ms;
          if (typeof x === 'string') ms = Date.parse(x);
          else if (typeof x === 'number') ms = x;
          else if (x && x.$numberLong !== undefined) ms = Number(x.$numberLong);
          const d = new Date(ms);
          // Dates JavaScript cannot hold (beyond +-8.64e15 ms) keep their Extended JSON.
          return Number.isNaN(d.getTime()) ? new Raw(v) : d;
        }
        case '$binary':
          if (x && typeof x === 'object') return new Binary(x.base64, x.subType);
          break;
        case '$uuid': return UUID(x);
        case '$timestamp': return new Timestamp(x.t, x.i);
        case '$regularExpression': {
          const flags = x.options || '';
          if (/^[imsu]*$/.test(flags)) {
            try { return new RegExp(x.pattern, flags); } catch (e) { /* kept raw below */ }
          }
          return new Raw(v);
        }
        case '$minKey': return new MinKey();
        case '$maxKey': return new MaxKey();
        case '$symbol':
        case '$undefined':
        case '$code':
        case '$dbPointer': return new Raw(v);
        default: break;
      }
    }
    const out = {};
    for (const key of keys) out[key] = revive(v[key]);
    return out;
  }

  const EJSON = {
    stringify(value, replacer, space, options) {
      if (replacer !== null && typeof replacer === 'object' && space === undefined && options === undefined) {
        options = replacer;
        replacer = undefined;
      }
      const relaxed = !(options && options.relaxed === false);
      return JSON.stringify(relaxed ? toRelaxed(value) : toEJSON(value), replacer, space);
    },
    parse(text) {
      return revive(JSON.parse(text));
    },
    serialize(value, options) {
      return options && options.relaxed === false ? toEJSON(value) : toRelaxed(value);
    },
    deserialize(value) {
      return revive(value);
    },
  };

  // ---- output -------------------------------------------------------------------------------

  function show(v) {
    if (typeof v === 'string') return v;
    if (v === undefined) return 'undefined';
    const r = toRelaxed(v);
    return typeof r === 'string' ? r : JSON.stringify(r);
  }

  function print(...args) {
    call('print', { text: args.map(show).join(' ') });
  }

  function printjson(v) {
    const r = toRelaxed(v);
    call('print', { text: r === undefined ? 'undefined' : JSON.stringify(r, null, 2) });
  }

  function sleep(ms) {
    call('sleep', { ms: Number(ms) || 0 });
  }

  // ---- database, collections, cursors -------------------------------------------------------

  let current = 'test';

  function command(database, doc) {
    if (!isDocument(doc)) fail('a command must be a document, such as {ping: 1}');
    return revive(call('command', { db: database, cmd: toEJSON(doc) }));
  }

  function checkCount(n, what) {
    if (typeof n !== 'number' && !(n instanceof Long) && !(n instanceof Int32)) fail(what + ' takes a number');
    return Number(n);
  }

  function checkFilter(filter, what) {
    if (filter === undefined || filter === null) return {};
    if (filter instanceof ObjectId) return { _id: filter };
    if (!isDocument(filter)) fail(what + ' takes a filter document');
    return filter;
  }

  // Writes need their filter: a missing one (undefined or null, e.g. a property that is not
  // set) would otherwise reach every document. `{}` says "all documents" explicitly.
  function requiredFilter(filter, what) {
    if (filter === undefined || filter === null) {
      fail(what + ' needs a filter document (use {} for every document)');
    }
    return checkFilter(filter, what);
  }

  // A count that must be a whole number >= 0 (skip, batchSize, maxTimeMS).
  function checkNonNegative(n, what) {
    const v = checkCount(n, what);
    if (!Number.isInteger(v) || v < 0) fail(what + ' takes a whole number of 0 or more');
    return v;
  }

  // An update for updateOne / updateMany: a non-empty document whose keys are all update
  // operators, or a non-empty aggregation pipeline (as mongosh and the drivers require).
  function checkUpdate(update, what) {
    if (Array.isArray(update)) {
      if (update.length === 0) fail(what + ' takes a non-empty update pipeline');
      return;
    }
    if (!isDocument(update)) fail(what + ' takes an update document or pipeline');
    const keys = Object.keys(update);
    if (keys.length === 0 || keys.some((k) => k[0] !== '$')) {
      fail(what + ' takes update operators ($set, ...); use replaceOne() to replace a document');
    }
  }

  function writeResult(reply, done) {
    const errors = reply.writeErrors;
    if (Array.isArray(errors) && errors.length > 0) {
      const w = errors[0];
      let message = w.errmsg;
      if (done) message += ' (' + done(Number(reply.n || 0), errors.length) + ')';
      const err = new MongoServerError(message, w.codeName || String(w.code), w.code);
      if (done) {
        err.nInserted = Number(reply.n || 0);
        err.writeErrorCount = errors.length;
      }
      throw err;
    }
    const wc = reply.writeConcernError;
    if (wc) throw new MongoServerError(wc.errmsg, wc.codeName || String(wc.code), wc.code);
    return reply;
  }

  // The document to insert: a missing `_id` (absent or explicitly undefined) gets a new
  // ObjectId, first, so insertedId reports what the server stored.
  function withId(doc) {
    if (doc._id !== undefined) return doc;
    const out = { _id: new ObjectId() };
    for (const key of Object.keys(doc)) {
      if (key !== '_id') out[key] = doc[key];
    }
    return out;
  }

  function WriteResult(fields, affected) {
    Object.assign(this, fields);
    Object.defineProperty(this, 'oxbaseAffected', { value: affected, enumerable: false });
  }

  function drainFirstBatch(reply) {
    return reply.cursor ? reply.cursor.firstBatch : [];
  }

  class Cursor {
    constructor(spec) {
      Object.defineProperty(this, '_spec', { value: spec, enumerable: false });
      Object.defineProperty(this, '_state', {
        value: { id: null, buffer: [], at: 0, done: false, started: false },
        enumerable: false,
      });
    }
    _option(key, value, kinds) {
      if (this._state.started) fail('the cursor has already been read; build a new one to change ' + key);
      if (!kinds.includes(this._spec.kind)) fail(key + '() is not available on ' + this._spec.kind + ' cursors');
      this._spec[key] = value;
      return this;
    }
    sort(spec) { if (!isDocument(spec)) fail('sort() takes a document'); return this._option('sort', spec, ['find']); }
    limit(n) { return this._option('limit', Math.abs(checkCount(n, 'limit()')), ['find']); }
    skip(n) { return this._option('skip', checkNonNegative(n, 'skip()'), ['find']); }
    project(spec) { if (!isDocument(spec)) fail('project() takes a document'); return this._option('projection', spec, ['find']); }
    projection(spec) { return this.project(spec); }
    hint(h) { return this._option('hint', h, ['find', 'aggregate']); }
    batchSize(n) { return this._option('batchSize', checkNonNegative(n, 'batchSize()'), ['find', 'aggregate']); }
    maxTimeMS(n) { return this._option('maxTimeMS', checkNonNegative(n, 'maxTimeMS()'), ['find', 'aggregate']); }
    collation(c) { if (!isDocument(c)) fail('collation() takes a document'); return this._option('collation', c, ['find', 'aggregate']); }
    allowDiskUse(b) { return this._option('allowDiskUse', b !== false, ['aggregate']); }
    comment() { return this; }
    pretty() { return this; }
    _open() {
      const s = this._state;
      if (s.started) return;
      s.started = true;
      const spec = this._spec;
      const payload = { db: spec.db, coll: spec.coll };
      for (const key of ['skip', 'limit', 'batchSize', 'maxTimeMS', 'allowDiskUse']) {
        if (spec[key] !== undefined) payload[key] = spec[key];
      }
      for (const key of ['filter', 'projection', 'sort', 'collation', 'pipeline']) {
        if (spec[key] !== undefined) payload[key] = toEJSON(spec[key]);
      }
      if (spec.hint !== undefined) payload.hint = typeof spec.hint === 'string' ? spec.hint : toEJSON(spec.hint);
      s.id = call(spec.kind, payload).id;
    }
    _fill() {
      const s = this._state;
      if (s.at < s.buffer.length || s.done) return;
      const r = call('next', { id: s.id, max: 1000 });
      s.buffer = revive(r.docs);
      s.at = 0;
      if (r.done) s.done = true;
    }
    hasNext() {
      this._open();
      this._fill();
      return this._state.at < this._state.buffer.length;
    }
    next() {
      if (!this.hasNext()) throw new Error('the cursor has no more documents (check hasNext() first)');
      const s = this._state;
      const doc = s.buffer[s.at];
      s.buffer[s.at] = undefined;
      s.at += 1;
      return doc;
    }
    tryNext() { return this.hasNext() ? this.next() : null; }
    toArray() {
      const out = [];
      while (this.hasNext()) out.push(this.next());
      return out;
    }
    forEach(fn) {
      if (typeof fn !== 'function') fail('forEach() takes a function');
      while (this.hasNext()) {
        if (fn(this.next()) === false) break;
      }
    }
    map(fn) {
      if (typeof fn !== 'function') fail('map() takes a function');
      return this.toArray().map((d) => fn(d));
    }
    itcount() {
      let n = 0;
      while (this.hasNext()) {
        this.next();
        n += 1;
      }
      return n;
    }
    count() {
      const spec = this._spec;
      if (spec.kind !== 'find') fail('count() is available on find cursors');
      return new Collection(spec.db, spec.coll).countDocuments(spec.filter || {});
    }
    close() {
      const s = this._state;
      if (s.started && !s.done) call('close', { id: s.id });
      s.done = true;
      s.buffer = [];
      s.at = 0;
    }
    isExhausted() { return this._state.started && this._state.done && this._state.at >= this._state.buffer.length; }
    toString() { return '[cursor]'; }
  }

  function indexName(keys) {
    return Object.keys(keys).map((k) => k + '_' + String(keys[k])).join('_');
  }

  function checkName(name, what) {
    if (typeof name !== 'string' || name.length === 0) fail(what + ' name must be a non-empty string');
    return name;
  }

  class Collection {
    constructor(database, name) {
      Object.defineProperty(this, '_db', { value: checkName(database, 'database'), enumerable: false });
      Object.defineProperty(this, '_name', { value: checkName(name, 'collection'), enumerable: false });
    }
    getName() { return this._name; }
    getFullName() { return this._db + '.' + this._name; }
    getDB() { return dbProxy(this._db); }
    toString() { return this.getFullName(); }
    _command(doc) { return command(this._db, doc); }
    find(filter, projection) {
      const spec = { kind: 'find', db: this._db, coll: this._name, filter: checkFilter(filter, 'find()') };
      if (projection !== undefined && projection !== null) {
        if (!isDocument(projection)) fail('find() takes a projection document');
        spec.projection = projection;
      }
      return new Cursor(spec);
    }
    findOne(filter, projection) {
      const doc = { find: this._name, filter: checkFilter(filter, 'findOne()'), limit: 1, singleBatch: true };
      if (projection !== undefined && projection !== null) doc.projection = projection;
      const batch = drainFirstBatch(this._command(doc));
      return batch.length > 0 ? batch[0] : null;
    }
    aggregate(pipeline, options) {
      if (!Array.isArray(pipeline)) {
        pipeline = Array.prototype.slice.call(arguments);
        options = undefined;
      }
      const spec = { kind: 'aggregate', db: this._db, coll: this._name, pipeline };
      if (options) {
        for (const key of ['allowDiskUse', 'collation', 'maxTimeMS', 'batchSize', 'hint']) {
          if (options[key] !== undefined) spec[key] = options[key];
        }
      }
      return new Cursor(spec);
    }
    insertOne(doc) {
      if (!isDocument(doc)) fail('insertOne() takes a document');
      const stored = withId(doc);
      writeResult(this._command({ insert: this._name, documents: [stored], ordered: true }));
      return new WriteResult({ acknowledged: true, insertedId: stored._id }, 1);
    }
    insertMany(docs, options) {
      if (!Array.isArray(docs) || !docs.every(isDocument)) fail('insertMany() takes an array of documents');
      const stored = docs.map(withId);
      const ordered = !(options && options.ordered === false);
      const reply = writeResult(
        this._command({ insert: this._name, documents: stored, ordered }),
        (n, failed) => 'insertMany() inserted ' + n + ' of ' + stored.length + ' documents; ' + failed + ' failed',
      );
      const insertedIds = {};
      stored.forEach((d, i) => { insertedIds[i] = d._id; });
      return new WriteResult({ acknowledged: true, insertedCount: Number(reply.n), insertedIds }, Number(reply.n));
    }
    _update(filter, update, options, multi, what) {
      if (!isDocument(update) && !Array.isArray(update)) fail(what + ' takes an update document or pipeline');
      const statement = { q: requiredFilter(filter, what), u: update, multi, upsert: !!(options && options.upsert) };
      if (options) {
        for (const key of ['arrayFilters', 'collation', 'hint']) {
          if (options[key] !== undefined) statement[key] = options[key];
        }
      }
      const reply = writeResult(this._command({ update: this._name, updates: [statement], ordered: true }));
      const upserted = Array.isArray(reply.upserted) ? reply.upserted : [];
      const matched = Number(reply.n) - upserted.length;
      const modified = Number(reply.nModified || 0);
      return new WriteResult({
        acknowledged: true,
        matchedCount: matched,
        modifiedCount: modified,
        upsertedCount: upserted.length,
        upsertedId: upserted.length > 0 ? upserted[0]._id : null,
      }, modified + upserted.length);
    }
    updateOne(filter, update, options) {
      requiredFilter(filter, 'updateOne()');
      checkUpdate(update, 'updateOne()');
      return this._update(filter, update, options, false, 'updateOne()');
    }
    updateMany(filter, update, options) {
      requiredFilter(filter, 'updateMany()');
      checkUpdate(update, 'updateMany()');
      return this._update(filter, update, options, true, 'updateMany()');
    }
    replaceOne(filter, replacement, options) {
      if (!isDocument(replacement) || Object.keys(replacement).some((k) => k[0] === '$')) {
        fail('replaceOne() takes a replacement document without update operators');
      }
      return this._update(filter, replacement, options, false, 'replaceOne()');
    }
    _delete(filter, options, limit, what) {
      const statement = { q: requiredFilter(filter, what), limit };
      if (options && options.collation) statement.collation = options.collation;
      if (options && options.hint) statement.hint = options.hint;
      const reply = writeResult(this._command({ delete: this._name, deletes: [statement], ordered: true }));
      return new WriteResult({ acknowledged: true, deletedCount: Number(reply.n) }, Number(reply.n));
    }
    deleteOne(filter, options) { return this._delete(filter, options, 1, 'deleteOne()'); }
    deleteMany(filter, options) { return this._delete(filter, options, 0, 'deleteMany()'); }
    countDocuments(filter, options) {
      const pipeline = [{ $match: checkFilter(filter, 'countDocuments()') }];
      if (options && options.skip) pipeline.push({ $skip: Number(options.skip) });
      if (options && options.limit) pipeline.push({ $limit: Number(options.limit) });
      pipeline.push({ $group: { _id: 1, n: { $sum: 1 } } });
      const batch = drainFirstBatch(this._command({ aggregate: this._name, pipeline, cursor: {} }));
      return batch.length > 0 ? Number(batch[0].n) : 0;
    }
    estimatedDocumentCount() {
      return Number(this._command({ count: this._name }).n);
    }
    distinct(key, filter) {
      if (typeof key !== 'string') fail('distinct() takes a field name');
      return this._command({ distinct: this._name, key, query: checkFilter(filter, 'distinct()') }).values;
    }
    createIndex(keys, options) {
      if (!isDocument(keys)) fail('createIndex() takes a key pattern document');
      const spec = Object.assign({ key: keys, name: indexName(keys) }, options || {});
      this._command({ createIndexes: this._name, indexes: [spec] });
      return spec.name;
    }
    createIndexes(list, options) {
      if (!Array.isArray(list)) fail('createIndexes() takes an array of key patterns');
      const specs = list.map((keys) => Object.assign({ key: keys, name: indexName(keys) }, options || {}));
      this._command({ createIndexes: this._name, indexes: specs });
      return specs.map((s) => s.name);
    }
    getIndexes() {
      return drainFirstBatch(this._command({ listIndexes: this._name, cursor: { batchSize: 100000 } }));
    }
    dropIndex(index) {
      if (index === undefined) fail('dropIndex() takes an index name or key pattern');
      return this._command({ dropIndexes: this._name, index });
    }
    dropIndexes(index) {
      return this._command({ dropIndexes: this._name, index: index === undefined ? '*' : index });
    }
    drop() {
      try {
        this._command({ drop: this._name });
        return true;
      } catch (e) {
        if (e && e.codeName === 'NamespaceNotFound') return false;
        throw e;
      }
    }
    renameCollection(target, dropTarget) {
      checkName(target, 'target collection');
      return command('admin', {
        renameCollection: this.getFullName(),
        to: this._db + '.' + target,
        dropTarget: !!dropTarget,
      });
    }
  }

  class Database {
    constructor(name) {
      Object.defineProperty(this, '_name', { value: checkName(name, 'database'), enumerable: false });
    }
    getName() { return this._name; }
    toString() { return this._name; }
    getSiblingDB(name) { return dbProxy(name); }
    getCollection(name) { return collectionProxy(this._name, name); }
    getCollectionInfos(filter, nameOnly) {
      const doc = { listCollections: 1, filter: filter || {}, nameOnly: !!nameOnly, authorizedCollections: true, cursor: { batchSize: 100000 } };
      return drainFirstBatch(command(this._name, doc));
    }
    getCollectionNames() {
      return this.getCollectionInfos({}, true).map((c) => c.name).sort();
    }
    runCommand(doc) {
      return command(this._name, typeof doc === 'string' ? { [doc]: 1 } : doc);
    }
    adminCommand(doc) {
      return command('admin', typeof doc === 'string' ? { [doc]: 1 } : doc);
    }
    createCollection(name, options) {
      return command(this._name, Object.assign({ create: checkName(name, 'collection') }, options || {}));
    }
    createView(name, source, pipeline, options) {
      return command(this._name, Object.assign({ create: checkName(name, 'view'), viewOn: checkName(source, 'source'), pipeline: pipeline || [] }, options || {}));
    }
    dropDatabase() { return command(this._name, { dropDatabase: 1 }); }
    stats(scale) { return command(this._name, { dbStats: 1, scale: scale || 1 }); }
    version() { return command('admin', { buildInfo: 1 }).version; }
    aggregate(pipeline, options) {
      const spec = { kind: 'aggregate', db: this._name, coll: null, pipeline };
      if (options) Object.assign(spec, options);
      return new Cursor(spec);
    }
  }

  const methodsOf = (proto) => (prop) => typeof prop !== 'string' || prop in proto || prop.startsWith('_');

  function collectionProxy(database, name) {
    const target = new Collection(database, name);
    const own = methodsOf(target);
    return new Proxy(target, {
      get(t, prop, receiver) {
        if (own(prop)) return Reflect.get(t, prop, receiver);
        return collectionProxy(database, name + '.' + prop);
      },
    });
  }

  function dbProxy(name) {
    const target = new Database(name);
    const own = methodsOf(target);
    return new Proxy(target, {
      get(t, prop, receiver) {
        if (own(prop)) return Reflect.get(t, prop, receiver);
        return collectionProxy(name, prop);
      },
    });
  }

  // ---- what a statement returned ------------------------------------------------------------

  function requestable(spec) {
    if (spec.coll === null) return false;
    if (spec.kind === 'aggregate') return spec.hint === undefined;
    return typeof spec.hint !== 'object' || spec.hint === null || isDocument(spec.hint);
  }

  function describe(v) {
    if (v === undefined) return JSON.stringify({ kind: 'none' });
    if (v instanceof Cursor) {
      const spec = v._spec;
      if (!v._state.started && requestable(spec)) {
        const request = { kind: spec.kind, db: spec.db, coll: spec.coll };
        for (const key of ['skip', 'limit', 'maxTimeMS', 'allowDiskUse']) {
          if (spec[key] !== undefined) request[key] = spec[key];
        }
        for (const key of ['filter', 'projection', 'sort', 'collation', 'pipeline']) {
          if (spec[key] !== undefined) request[key] = toEJSON(spec[key]);
        }
        if (spec.hint !== undefined) request.hint = typeof spec.hint === 'string' ? spec.hint : toEJSON(spec.hint);
        return JSON.stringify({ kind: 'request', request });
      }
      return JSON.stringify({ kind: 'documents', docs: v.toArray().map((d) => toEJSON(d)) });
    }
    if (v instanceof WriteResult) {
      const row = toEJSON(Object.assign({}, v));
      return JSON.stringify({ kind: 'write', affected: v.oxbaseAffected, row });
    }
    if (isDocument(v)) return JSON.stringify({ kind: 'documents', docs: [toEJSON(v)] });
    if (Array.isArray(v) && v.length > 0 && v.every(isDocument)) {
      return JSON.stringify({ kind: 'documents', docs: v.map((d) => toEJSON(d)) });
    }
    const e = toEJSON(v);
    return JSON.stringify({ kind: 'value', value: e === undefined ? null : e });
  }

  // ---- globals ------------------------------------------------------------------------------

  function hidden(name, value) {
    Object.defineProperty(G, name, { value, writable: false, enumerable: false, configurable: false });
  }

  hidden('__oxbase_describe', describe);
  hidden('__oxbase_begin', (name) => { current = name; });

  Object.defineProperty(G, 'db', {
    get() { return dbProxy(current); },
    set(value) {
      if (!(value instanceof Database)) fail('db can only be set to a database: db = db.getSiblingDB("name")');
      call('use', { db: value.getName() });
      current = value.getName();
    },
    enumerable: true,
    configurable: false,
  });

  const globals = {
    ObjectId,
    ISODate,
    NumberInt: Int32,
    NumberLong: Long,
    NumberDecimal: Decimal128,
    Long,
    Int32,
    Decimal128,
    Timestamp,
    BinData,
    Binary,
    UUID,
    MinKey,
    MaxKey,
    EJSON,
    print,
    printjson,
    sleep,
    console: Object.freeze({ log: print, info: print, warn: print, error: print }),
  };
  for (const key of Object.keys(globals)) {
    Object.defineProperty(G, key, { value: globals[key], writable: true, enumerable: false, configurable: true });
  }
})(globalThis.__oxbase_host);
