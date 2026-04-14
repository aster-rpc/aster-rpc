import { WireType } from '@aster-rpc/aster';

// Self-referential @WireType: Entry contains a list of Entries (a tree).
// Exercises the Tarjan SCC / SELF_REF path in aster-gen.
@WireType('tree/Entry')
export class Entry {
  name = '';
  children: Entry[] = [];
}

// Root query: ask the server for a tree and get a tree back. Both
// request and response reference Entry, so the aster-gen output should
// contain a real requestTypeHash/responseTypeHash (not zeros).
@WireType('tree/TreeRequest')
export class TreeRequest {
  root: Entry = new Entry();
}

@WireType('tree/TreeResponse')
export class TreeResponse {
  tree: Entry = new Entry();
}
