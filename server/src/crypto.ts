// SPDX-License-Identifier: AGPL-3.0-or-later

import { isUuidV7 } from "./ids";

const encoder = new TextEncoder();

export async function sha256(data: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.digest("SHA-256", data));
}

export function base64url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

export function fromBase64url(text: string): Uint8Array | undefined {
  if (!/^[A-Za-z0-9_-]*$/.test(text)) return undefined;
  try {
    const binary = atob(text.replaceAll("-", "+").replaceAll("_", "/"));
    return Uint8Array.from(binary, (c) => c.charCodeAt(0));
  } catch {
    return undefined;
  }
}

function importHmacKey(secret: Uint8Array): Promise<CryptoKey> {
  return crypto.subtle.importKey("raw", secret, { name: "HMAC", hash: "SHA-256" }, false, ["sign", "verify"]);
}

export async function hmac(secret: Uint8Array, message: string): Promise<Uint8Array> {
  const key = await importHmacKey(secret);
  return new Uint8Array(await crypto.subtle.sign("HMAC", key, encoder.encode(message)));
}

export async function hmacVerify(secret: Uint8Array, message: string, mac: Uint8Array): Promise<boolean> {
  const key = await importHmacKey(secret);
  return crypto.subtle.verify("HMAC", key, mac, encoder.encode(message));
}

export function deriveSpaceKey(serverKey: Uint8Array, spaceId: string): Promise<Uint8Array> {
  return hmac(serverKey, `koidrive/space-key/v1|${spaceId}`);
}

export type ChunkGrant = "put" | "get" | "receipt";

export interface ChunkObject {
  spaceId: string;
  hashHex: string;
  incarnation: string;
}

function grantMessage(grant: ChunkGrant, chunk: ChunkObject, size: number, expiresAtMs: number): string {
  return `${grant}|${chunk.spaceId}|${chunk.hashHex}|${chunk.incarnation}|${size}|${expiresAtMs}`;
}

async function signV1(spaceKey: Uint8Array, message: string, expiresAtMs: number): Promise<string> {
  return `v1.${expiresAtMs}.${base64url(await hmac(spaceKey, message))}`;
}

async function verifyV1(spaceKey: Uint8Array, token: string, nowMs: number, messageAt: (expiresAtMs: number) => string): Promise<boolean> {
  const [version, expires, macText, ...rest] = token.split(".");
  if (version !== "v1" || rest.length > 0 || !expires || !/^\d+$/.test(expires) || macText === undefined) return false;
  const expiresAtMs = Number(expires);
  if (expiresAtMs < nowMs) return false;
  const mac = fromBase64url(macText);
  if (!mac) return false;
  return hmacVerify(spaceKey, messageAt(expiresAtMs), mac);
}

export function signChunkGrant(spaceKey: Uint8Array, grant: ChunkGrant, chunk: ChunkObject, size: number, expiresAtMs: number): Promise<string> {
  return signV1(spaceKey, grantMessage(grant, chunk, size, expiresAtMs), expiresAtMs);
}

export function verifyChunkGrant(spaceKey: Uint8Array, grant: ChunkGrant, chunk: ChunkObject, size: number, token: string, nowMs: number): Promise<boolean> {
  return verifyV1(spaceKey, token, nowMs, (expiresAtMs) => grantMessage(grant, chunk, size, expiresAtMs));
}

export interface PackLocation {
  packId: string;
  offset: number;
  packSize: number;
}

function packedReceiptMessage(chunk: ChunkObject, size: number, expiresAtMs: number, at: PackLocation): string {
  return `packed|${chunk.spaceId}|${chunk.hashHex}|${chunk.incarnation}|${size}|${expiresAtMs}|${at.packId}|${at.offset}|${at.packSize}`;
}

export async function signPackedReceipt(spaceKey: Uint8Array, chunk: ChunkObject, size: number, at: PackLocation, expiresAtMs: number): Promise<string> {
  const mac = await hmac(spaceKey, packedReceiptMessage(chunk, size, expiresAtMs, at));
  return `v2.${expiresAtMs}.${at.packId}.${at.offset}.${at.packSize}.${base64url(mac)}`;
}

export async function verifyPackedReceipt(
  spaceKey: Uint8Array,
  chunk: ChunkObject,
  size: number,
  token: string,
  nowMs: number,
): Promise<{ at: PackLocation; expired: boolean } | undefined> {
  const [version, expires, packId, offset, packSize, macText, ...rest] = token.split(".");
  const numbers = [expires, offset, packSize];
  if (version !== "v2" || rest.length > 0 || macText === undefined || !isUuidV7(packId ?? "") || !numbers.every((text) => /^\d{1,15}$/.test(text ?? ""))) return undefined;
  const mac = fromBase64url(macText);
  if (!mac) return undefined;
  const at = { packId: packId!, offset: Number(offset), packSize: Number(packSize) };
  const expiresAtMs = Number(expires);
  if (!(await hmacVerify(spaceKey, packedReceiptMessage(chunk, size, expiresAtMs, at), mac))) return undefined;
  return { at, expired: expiresAtMs < nowMs };
}

export interface PackedRead {
  spaceId: string;
  hashHex: string;
  packId: string;
  offset: number;
  size: number;
}

function packedGetMessage(read: PackedRead, expiresAtMs: number): string {
  return `packed-get|${read.spaceId}|${read.hashHex}|${read.packId}|${read.offset}|${read.size}|${expiresAtMs}`;
}

export function signPackedGet(spaceKey: Uint8Array, read: PackedRead, expiresAtMs: number): Promise<string> {
  return signV1(spaceKey, packedGetMessage(read, expiresAtMs), expiresAtMs);
}

export function verifyPackedGet(spaceKey: Uint8Array, read: PackedRead, token: string, nowMs: number): Promise<boolean> {
  return verifyV1(spaceKey, token, nowMs, (expiresAtMs) => packedGetMessage(read, expiresAtMs));
}
