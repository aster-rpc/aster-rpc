import { Service, Rpc } from '@aster-rpc/aster';
import { Alpha, Author, Book, Forest, Gamma, Identified, Priced, Temporal, TreeNode } from './types.js';

@Service({ name: 'Types', version: 1 })
export class TypesService {
  @Rpc()
  async echo_temporal(req: Temporal): Promise<Temporal> {
    return req;
  }

  @Rpc()
  async plant(_req: TreeNode): Promise<Forest> {
    return new Forest();
  }

  @Rpc()
  async author(_req: Book): Promise<Author> {
    return new Author();
  }

  @Rpc()
  async cycle(_req: Alpha): Promise<Gamma> {
    return new Gamma();
  }
}

@Service({ name: 'Ids', version: 1 })
export class IdsService {
  @Rpc()
  async price(_req: Identified): Promise<Priced> {
    return new Priced();
  }
}
