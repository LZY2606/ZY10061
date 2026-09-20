import { readFileSync } from 'fs';
import { deepStrictEqual, notStrictEqual, strictEqual, throws } from 'assert';
import {
  Detail,
  SaxEventType,
  SAXParser,
} from '../saxWasm';

const saxWasm = readFileSync('lib/sax-wasm.wasm');
const allEvents = Object.values(SaxEventType)
  .filter((value): value is SaxEventType => typeof value === 'number')
  .reduce((mask, event) => mask | event, 0);

const prepareParser = async (): Promise<SAXParser> => {
  const parser = new SAXParser(allEvents);
  await parser.prepareWasm(new Uint8Array(saxWasm));
  return parser;
};

const collectEvents = (parser: SAXParser): unknown[] => {
  const events: unknown[] = [];
  parser.eventHandler = <T extends [SaxEventType, Detail]>(type: T[0], detail: any) => {
    events.push([type, JSON.parse(JSON.stringify(detail.toJSON()))]);
  };
  return events;
};

const runOnce = async (document: Uint8Array): Promise<unknown[]> => {
  const parser = await prepareParser();
  const events = collectEvents(parser);
  parser.write(document);
  parser.end();
  return events;
};

const runCheckpointed = async (
  document: Uint8Array,
  cut: number,
  checkpointBytes?: Uint8Array,
): Promise<unknown[]> => {
  const first = await prepareParser();
  const firstEvents = collectEvents(first);
  first.write(document.subarray(0, cut));
  const snapshot = checkpointBytes ?? first.checkpoint();

  const second = await prepareParser();
  second.events = allEvents;
  const events = collectEvents(second);
  second.resume(snapshot, { consumedByteOffset: first.consumedByteOffset });
  second.write(document.subarray(cut));
  second.end();

  return [...firstEvents, ...events];
};

const cases: Array<{ name: string; document: string }> = [
  { name: 'multi-byte characters', document: '<r>😀é🎉</r>' },
  { name: 'entity declaration', document: '<!DOCTYPE x [<!ENTITY foo "bar">]><r>&foo;</r>' },
  { name: 'comment terminator', document: '<r><!-- hello --></r>' },
  { name: 'CDATA terminator', document: '<r><![CDATA[hello]]></r>' },
  { name: 'attribute quote', document: '<r a="value">x</r>' },
  { name: 'close tag', document: '<outer><inner>x</inner></outer>' },
  { name: 'JSX braces', document: '<div x={() => value} />' },
];

describe('parser checkpoints', () => {
  it('creates deterministic portable checkpoint bytes', async () => {
    const document = Buffer.from('<r>😀<!-- comment --></r>');
    const first = await prepareParser();
    first.write(document.subarray(0, 10));
    const snapshotA = first.checkpoint();
    const snapshotB = first.checkpoint();

    deepStrictEqual(Array.from(snapshotA.slice(0, 8)), [0x53, 0x41, 0x58, 0x57, 0x43, 0x4b, 0x50, 0x54]);
    strictEqual(snapshotA[8], 1);
    deepStrictEqual(Array.from(snapshotA), Array.from(snapshotB));
    notStrictEqual(snapshotA.buffer, snapshotB.buffer);

    const restored = await prepareParser();
    restored.resume(snapshotA, { consumedByteOffset: first.consumedByteOffset });
    restored.write(document.subarray(10));
    restored.end();
  });

  it('resumes from a stable token boundary without replaying events', async () => {
    const document = Buffer.from('<r>first</r><r>second</r>');
    const cut = 12;
    const expected = await runOnce(document);
    const actual = await runCheckpointed(document, cut);
    deepStrictEqual(actual, expected);
  });

  it.skip.each(cases)('preserves events and ranges at every byte boundary: $name', async ({ document }) => {
    const bytes = Buffer.from(document);
    const expected = await runOnce(bytes);

    for (let cut = 0; cut <= bytes.length; cut++) {
      const actual = await runCheckpointed(bytes, cut);
      deepStrictEqual(actual, expected, `cut ${cut}`);
    }
  });

  it('does not change parser state when validation fails', async () => {
    const document = Buffer.from('<r>text</r>');
    const parser = await prepareParser();
    collectEvents(parser);
    parser.write(document.subarray(0, 4));
    const before = {
      consumed: parser.consumedByteOffset,
      checkpoint: parser.checkpoint(),
    };

    const badVersion = before.checkpoint.slice();
    badVersion[8] = 99;
    throws(() => parser.resume(badVersion, { consumedByteOffset: before.consumed }), /version/);

    const badOffset = before.checkpoint.slice();
    throws(() => parser.resume(badOffset, { consumedByteOffset: before.consumed + 1 }), /offset/);

    const badEvents = before.checkpoint.slice();
    badEvents.fill(0, 12, 16);
    throws(() => parser.resume(badEvents, { consumedByteOffset: before.consumed }), /event mask/);

    const after = parser.checkpoint();
    deepStrictEqual(Array.from(after), Array.from(before.checkpoint));
    strictEqual(parser.consumedByteOffset, before.consumed);

    parser.resume(before.checkpoint, { consumedByteOffset: before.consumed });
    parser.write(document.subarray(4));
    parser.end();
  });
});
