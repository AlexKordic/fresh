import { Utf8Offsets } from "../lib/utf8_offsets.ts";

let failures = 0;
function eq(actual: unknown, expected: unknown, name: string): void {
  if (actual !== expected) { console.log(`FAIL ${name}: ${actual} != ${expected}`); failures++; }
}
const encoder = new TextEncoder();
for (const text of ["", "ascii\r\n", "café 東京", "👩🏽‍💻e\u0301🇺🇸", "**prose** [café](target.md)\r\n"]) {
  const index = new Utf8Offsets(text);
  let at = 0;
  eq(index.at(0), 0, "start");
  for (const scalar of text) {
    if (scalar.length === 2) eq(index.at(at + 1), null, "surrogate interior");
    at += scalar.length;
    eq(index.at(at), encoder.encode(text.slice(0, at)).length, `boundary ${at}`);
  }
  for (const invalid of [-1, text.length + 1, 0.5, NaN, Infinity]) eq(index.at(invalid), null, "invalid offset");
}
for (const text of ["bad\ud800tail", "bad\udc00tail"]) {
  const index = new Utf8Offsets(text);
  eq(index.at(0), null, "malformed source uses caller fallback");
  eq(index.at(text.length), null, "malformed end uses caller fallback");
}
const long = "Measured **prose** café 👩🏽‍💻 東京 with source offsets. ".repeat(7000);
const index = new Utf8Offsets(long);
for (const at of [0, 1, 4096, long.length / 2, long.length - 1, long.length]) {
  const first = long.charCodeAt(at), previous = long.charCodeAt(at - 1);
  const interior = first >= 0xdc00 && first <= 0xdfff && previous >= 0xd800 && previous <= 0xdbff;
  eq(index.at(at), interior ? null : encoder.encode(long.slice(0, at)).length, `long boundary ${at}`);
}
if (failures) throw new Error(`${failures} UTF-8 offset failures`);
console.log("PASS exact UTF-8 offsets: Unicode, CRLF, malformed/interior fallback and long source");
