import { WireType, Service, Rpc, i32 } from '@aster-rpc/aster';

// conformance/vectors/contract-types.json `field_names.java_defined`: the
// Orders contract as Java defines it. TypeScript declares the same camelCase
// names, so its contract names are the property names, verbatim.

@WireType('names.v1/Order')
export class Order {
  orderId = '';
  lineItems: LineItem[] = [];
  totalCents: i32 = i32(0);
  note: string | null = null;
}

@WireType('names.v1/LineItem')
export class LineItem {
  skuCode = '';
  unitCount: i32 = i32(0);
}

@Service({ name: 'Orders', version: 1 })
export class OrdersService {
  @Rpc()
  async place(req: Order): Promise<Order> {
    return req;
  }
}
