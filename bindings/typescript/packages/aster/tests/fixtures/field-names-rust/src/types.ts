import { WireType, WireName, Service, Rpc, i32 } from '@aster-rpc/aster';

// conformance/vectors/contract-types.json `field_names.rust_defined`: the
// Orders contract as Rust defines it (order_id). TypeScript keeps idiomatic
// camelCase properties and names each field with @WireName.

@WireType('names.v1/Order')
export class Order {
  @WireName('order_id') orderId = '';
  @WireName('line_items') lineItems: LineItem[] = [];
  @WireName('total_cents') totalCents: i32 = i32(0);
  note: string | null = null;
}

@WireType('names.v1/LineItem')
export class LineItem {
  @WireName('sku_code') skuCode = '';
  @WireName('unit_count') unitCount: i32 = i32(0);
}

@Service({ name: 'Orders', version: 1 })
export class OrdersService {
  @Rpc()
  async place(req: Order): Promise<Order> {
    return req;
  }
}
