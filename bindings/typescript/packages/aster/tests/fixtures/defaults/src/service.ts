import { Service, Rpc } from '@aster-rpc/aster';
import { Settings } from './types.js';

@Service({ name: 'Defaults', version: 1 })
export class DefaultsService {
  @Rpc()
  async put(req: Settings): Promise<Settings> {
    return req;
  }
}
