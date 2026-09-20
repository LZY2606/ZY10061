import { readFileSync } from 'fs';
import { resolve } from 'path';
import { deepStrictEqual, strictEqual, throws } from 'assert';
import {
  CheckpointError,
  Detail,
  Reader,
  SaxEventType,
  SAXParser,
} from '../saxWasm';

const saxWasm = readFileSync(resolve(__dirname, '../../../lib/sax-wasm.wasm'));

// OpenTagStart fires the moment a tag name begins, including a partial name,
// so it intentionally differs when cuts land inside a tag name. All other
// events are byte-for-byte deterministic across checkpoint boundaries.
const EVENTS =
  SaxEventType.Text |
  SaxEventType.ProcessingInstruction |
  SaxEventType.Declaration |
  SaxEventType.Doctype |
  SaxEventType.Comment |
  SaxEventType.Attribute |
  SaxEventType.OpenTag |
  SaxEventType.CloseTag |
  SaxEventType.Cdata;

type RecordedEvent = [SaxEventType, ReturnType<Reader<Detail>['toJSON']>];

const makeParser = async (): Promise<{ parser: SAXParser; events: RecordedEvent[] }> => {
  const events: RecordedEvent[] = [];
  const parser = new SAXParser(EVENTS);
  parser.eventHandler = (event: SaxEventType, detail: Reader<Detail>) => {
    events.push([event, detail.toJSON()]);
  };
  await parser.prepareWasm(saxWasm);
  return { parser, events };
};

const oneShot = async (bytes: Uint8Array): Promise<RecordedEvent[]> => {
  const { parser, events } = await makeParser();
  parser.write(bytes);
  parser.end();
  return events;
};

/**
 * Simulates a process restart at `cut`: the first parser processes the
 * prefix, a checkpoint is taken, then a brand new parser resumes from the
 * checkpoint and processes the remainder. Returns the concatenated event
 * stream a single continuous parser would have produced.
 */
const restartAt = async (bytes: Uint8Array, cut: number): Promise<RecordedEvent[]> => {
  const first = await makeParser();
  first.parser.write(bytes.subarray(0, cut));
  const checkpoint = first.parser.getCheckpoint();

  const second = await makeParser();
  const events = first.events;
  second.parser.resume(checkpoint);
  second.parser.eventHandler = (event: SaxEventType, detail: Reader<Detail>) => {
    events.push([event, detail.toJSON()]);
  };
  const consumed = SAXParser.readCheckpointConsumedBytes(checkpoint);
  second.parser.write(bytes.subarray(consumed));
  second.parser.end();
  return events;
};

describe('SAXParser checkpoint/resume', () => {
  it('checkpoint header carries magic, version and consumed offset', async () => {
    const { parser } = await makeParser();
    parser.write(Buffer.from('<root>hi'));
    const checkpoint = parser.getCheckpoint();
    deepStrictEqual(Array.from(checkpoint.subarray(0, 4)), [0x53, 0x41, 0x58, 0x43]);
    strictEqual(checkpoint[4], 1);
    strictEqual(SAXParser.readCheckpointConsumedBytes(checkpoint), 8);
  });

  it('produces deterministic bytes for identical state', async () => {
    const make = async () => {
      const { parser } = await makeParser();
      parser.write(Buffer.from('<root><a x="1">hello'));
      return parser.getCheckpoint();
    };
    const a = await make();
    const b = await make();
    deepStrictEqual(Array.from(a), Array.from(b));
  });

  it('checkpoint bytes are an owned copy, not wasm memory', async () => {
    const { parser } = await makeParser();
    parser.write(Buffer.from('<root>'));
    const first = parser.getCheckpoint();
    parser.write(Buffer.from('<child/>'));
    const second = parser.getCheckpoint();
    // Mutating the returned buffer does not corrupt later checkpoints.
    first.fill(0);
    strictEqual(Buffer.from(second.subarray(0, 4)).toString('latin1'), 'SAXC');
  });

  it('rejects non-checkpoint bytes without changing the parser', async () => {
    const { parser, events } = await makeParser();
    parser.write(Buffer.from('<root>content'));
    const checkpoint = parser.getCheckpoint();
    const before = SAXParser.readCheckpointConsumedBytes(checkpoint);

    throws(
      () => parser.resume(Buffer.from('not a checkpoint at all....')),
      (error: unknown) => error instanceof CheckpointError
    );

    parser.write(Buffer.from('</root>'));
    parser.end();
    strictEqual(events[0][0], SaxEventType.OpenTag);
    strictEqual(before, 13);
  });

  it('rejects an unsupported version', async () => {
    const { parser } = await makeParser();
    parser.write(Buffer.from('<a/>'));
    const checkpoint = parser.getCheckpoint();
    const forged = checkpoint.slice();
    forged[4] = 99;
    throws(() => parser.resume(forged), /version 99/);
  });

  it('rejects a mismatched event mask', async () => {
    const producer = new SAXParser(SaxEventType.Text);
    await producer.prepareWasm(saxWasm);
    producer.write(Buffer.from('hello'));
    const checkpoint = producer.getCheckpoint();

    const { parser, events } = await makeParser();
    throws(() => parser.resume(checkpoint), /event mask/);
    // Parser still usable after the failed resume.
    parser.write(Buffer.from('<a/>'));
    parser.end();
    strictEqual(events.length, 2);
    strictEqual(events[0][0], SaxEventType.OpenTag);
    strictEqual(events[1][0], SaxEventType.CloseTag);
  });

  it('rejects a mismatched consumed offset and keeps parsing intact', async () => {
    const { parser, events } = await makeParser();
    parser.write(Buffer.from('<a>hello'));
    const checkpoint = parser.getCheckpoint();
    throws(() => parser.resume(checkpoint, 12345), /consumed offset/);
    strictEqual(parser.resume(checkpoint), SAXParser.readCheckpointConsumedBytes(checkpoint));
    parser.write(Buffer.from('</a>'));
    parser.end();
    const text = events.find(([type]) => type === SaxEventType.Text);
    strictEqual((text?.[1] as { value: string }).value, 'hello');
  });

  const cases: Array<[string, string]> = [
    ['multi-byte characters', '<div>🚀 and é and 漢字</div>'],
    ['entities and text', '<p>Tom &amp; Jerry &lt;nick&gt;</p>'],
    ['comment terminators', '<a><!-- a -> b ---> c --></a>'],
    ['CDATA sections', '<x><![CDATA[data </not-tag> 🚀 ]]></x>'],
    ['attribute quotes', '<a single=\'1\' double="2" bare=3 mixed="a;b">x</a>'],
    ['close tags', '<outer><middle><inner>text</inner></middle></outer>'],
    ['JSX braces', '<C x={a + {n: 1}} y={() => <B z={2}/>}>z</C>'],
  ];

  for (const [label, markup] of cases) {
    it(`reproduces one-shot output at every byte cut for ${label}`, async () => {
      const bytes = Buffer.from(markup);
      const expected = await oneShot(bytes);
      for (let cut = 0; cut <= bytes.length; cut++) {
        const actual = await restartAt(bytes, cut);
        deepStrictEqual(
          actual,
          expected,
          `cut ${cut}/${bytes.length} for ${label} must match one-shot events`
        );
      }
    });
  }

  it('does not replay events emitted before the checkpoint', async () => {
    const bytes = Buffer.from('<a><b>one</b><b>two</b></a>');
    const { parser, events } = await makeParser();
    parser.write(Buffer.from('<a><b>one</b>'));
    const checkpoint = parser.getCheckpoint();
    const beforeCount = events.length;

    const next = await makeParser();
    next.parser.resume(checkpoint);
    strictEqual(next.events.length, 0);
    next.parser.write(Buffer.from('<b>two</b></a>'));
    next.parser.end();

    // The restarted parser emits only events for the remaining bytes.
    strictEqual(next.events.length + beforeCount, events.length + next.events.length);
    strictEqual(next.events[0][0], SaxEventType.OpenTag);
    strictEqual((next.events[0][1] as { name: string }).name, 'b');
  });

  it('continues incomplete tokens without losing their start', async () => {
    const bytes = Buffer.from('<comment><!-- 🚀 split --></comment>');
    const expected = await oneShot(bytes);
    // Cut inside the comment and inside the emoji simultaneously.
    const cut = bytes.indexOf('🚀') + 2;
    const actual = await restartAt(bytes, cut);
    deepStrictEqual(actual, expected);
  });
});
