import { Service, Rpc } from '@aster-rpc/aster';
import { TreeRequest, TreeResponse } from './types.js';

@Service({ name: 'TreeService', version: 1 })
export class TreeServiceImpl {
  @Rpc()
  async fetchTree(req: TreeRequest): Promise<TreeResponse> {
    void req;
    return new TreeResponse();
  }
}
