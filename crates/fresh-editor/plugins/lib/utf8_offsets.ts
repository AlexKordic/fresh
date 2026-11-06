/** Exact UTF-16 boundary -> UTF-8 byte offsets for one source string.
 * Interior surrogate boundaries and malformed strings deliberately return
 * null so callers can retain their original conversion behavior. */
export class Utf8Offsets {
  private readonly offsets: Float64Array | null;

  constructor(text: string) {
    const offsets = new Float64Array(text.length + 1);
    let bytes = 0;
    for (let i = 0; i < text.length;) {
      offsets[i] = bytes;
      const first = text.charCodeAt(i);
      if (first >= 0xd800 && first <= 0xdbff) {
        const second = text.charCodeAt(i + 1);
        if (!(second >= 0xdc00 && second <= 0xdfff)) { this.offsets = null; return; }
        offsets[i + 1] = -1;
        bytes += 4;
        i += 2;
      } else {
        if (first >= 0xdc00 && first <= 0xdfff) { this.offsets = null; return; }
        bytes += first < 0x80 ? 1 : first < 0x800 ? 2 : 3;
        i++;
      }
    }
    offsets[text.length] = bytes;
    this.offsets = offsets;
  }

  at(index: number): number | null {
    if (!this.offsets || !Number.isInteger(index) || index < 0 || index >= this.offsets.length) return null;
    const value = this.offsets[index];
    return value < 0 ? null : value;
  }
}
