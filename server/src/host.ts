// SPDX-License-Identifier: AGPL-3.0-or-later

export type SqlValue = ArrayBuffer | string | number | null;

export type SqlBinding = SqlValue | Uint8Array;

export type SqlRow = Record<string, SqlValue>;

export interface SqlCursor<T extends SqlRow> extends Iterable<T> {
  toArray(): T[];
  one(): T;
  readonly rowsRead: number;
  /** Rows the statement wrote, read after the cursor is consumed. */
  readonly rowsWritten: number;
}

/** A synchronous SQLite connection: every statement runs to completion before exec returns. */
export interface Sql {
  exec<T extends SqlRow = SqlRow>(query: string, ...bindings: SqlBinding[]): SqlCursor<T>;
  readonly databaseSize: number;
}

export interface Database {
  readonly sql: Sql;
  transactionSync<T>(closure: () => T): T;
}

export interface StoredObject {
  readonly key: string;
  readonly size: number;
  readonly uploaded: Date;
  /** Checksums the store computed or was given at upload; sha256 is absent when it has none. */
  readonly checksums: { readonly sha256?: ArrayBuffer };
}

export interface StoredObjectBody extends StoredObject {
  readonly body: ReadableStream<Uint8Array>;
}

export type ObjectList = { objects: StoredObject[] } & ({ truncated: true; cursor: string } | { truncated: false });

export interface PutOptions {
  /** A body with another digest is refused with an error that says "checksum". */
  sha256?: string;
  /** With etagDoesNotMatch "*", the put writes only when no object has the key and otherwise leaves the stored object. */
  onlyIf?: { etagDoesNotMatch: string };
}

export interface ObjectStore {
  head(key: string): Promise<StoredObject | null>;
  get(key: string): Promise<StoredObjectBody | null>;
  put(key: string, value: ReadableStream | ArrayBuffer | ArrayBufferView | string | null, options?: PutOptions): Promise<unknown>;
  list(options: { prefix: string; cursor?: string; limit: number }): Promise<ObjectList>;
  delete(keys: string | string[]): Promise<void>;
}

/** The attachment is a small value kept with the socket across hibernation. */
export interface Socket {
  readonly readyState: number;
  send(message: ArrayBuffer | ArrayBufferView | string): void;
  close(code?: number, reason?: string): void;
  serializeAttachment(value: unknown): void;
  deserializeAttachment(): unknown;
}

export interface ObjectStorage extends Database {
  sync(): Promise<void>;
  /** Runs the object's alarm handler at scheduledTimeMs, replacing any earlier setting. */
  setAlarm(scheduledTimeMs: number): Promise<void>;
  deleteAlarm(): Promise<void>;
}

export interface ObjectState {
  readonly id: { toString(): string };
  readonly storage: ObjectStorage;
  getWebSockets(tag?: string): Socket[];
  blockConcurrencyWhile<T>(callback: () => Promise<T>): Promise<T>;
  /** Ends the object's current instance; the next event starts a fresh one from storage. */
  abort(reason?: string): void;
}
