import {
  WireType, Decimal, i32, uuid, ulid, date,
  type decimal,
} from '@aster-rpc/aster';

// TypeScript twins of the Rust types in aster/tests/it/contract_types.rs;
// must hash to conformance/vectors/contract-types.json and read / write
// its payloads.

@WireType('types.v1/Temporal')
export class Temporal {
  at: Date = new Date(0);
  day: date = date('1970-01-01');
  amount: Decimal = new Decimal(0n);
  maybe_at: Date | null = null;
  history: Date[] = [];
}

@WireType('types.v1/TreeNode')
export class TreeNode {
  value = '';
  children: TreeNode[] = [];
}

@WireType('types.v1/Chain')
export class Chain {
  value: i32 = i32(0);
  next: Chain | null = null;
}

@WireType('types.v1/Author')
export class Author {
  name = '';
  books: Book[] = [];
}

@WireType('types.v1/Book')
export class Book {
  title = '';
  written_by: Author | null = null;
}

@WireType('types.v1/Alpha')
export class Alpha {
  next: Beta | null = null;
}

@WireType('types.v1/Beta')
export class Beta {
  next: Gamma | null = null;
}

@WireType('types.v1/Gamma')
export class Gamma {
  next: Alpha[] = [];
}

@WireType('types.v1/Forest')
export class Forest {
  trees: TreeNode[] = [];
}

@WireType('types.v1/Identified')
export class Identified {
  id: uuid = uuid('00000000-0000-0000-0000-000000000000');
  trace: ulid = ulid('00000000000000000000000000');
  parent: uuid | null = null;
  history: ulid[] = [];
  names: Map<uuid, string> = new Map();
}

@WireType('types.v1/Priced')
export class Priced {
  price: decimal<18, 4> = new Decimal(0n);
  rate: decimal<5, 0> = new Decimal(0n);
  maybe_price: decimal<18, 4> | null = null;
  amount: Decimal = new Decimal(0n);
}

// The spec's B.4 diamond: A -> C is a back-edge though C is hashed first.
@WireType('types.v1/DiamondA')
export class DiamondA {
  b: DiamondB | null = null;
  c: DiamondC | null = null;
}

@WireType('types.v1/DiamondB')
export class DiamondB {
  c: DiamondC | null = null;
}

@WireType('types.v1/DiamondC')
export class DiamondC {
  a: DiamondA[] = [];
}

// A cycle whose spanning tree depends on codepoint (not UTF-16) order:
// Ａ (U+FF21) and 𝔸 (U+1D538). Contract identity only, as in Rust.
@WireType('types.v1/Ord')
export class CpOrd {
  wide: CpWide | null = null;
  math: CpMath | null = null;
}

@WireType('types.v1/\uFF21')
export class CpWide {
  back: CpOrd | null = null;
  math: CpMath | null = null;
}

@WireType('types.v1/\u{1D538}')
export class CpMath {
  wide: CpWide | null = null;
}
