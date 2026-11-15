





import truth from '../truth.js';
import {total} from './helper.mts';



const label = 'modules';

function settle(entries) {
  return total(entries, () => truth.checkpoint(label, {locals: {entries}}));
}

settle([4, 5, 6]);
truth.release();
