"""Extract pinned pure admission bodies; replace only Anchor error plumbing/types."""
from pathlib import Path
r=Path(__file__).resolve().parents[1]
def function(file,name):
 s=(r/'reference/instructions'/file).read_text();start=s.index('fn '+name+'(');a=s.index('{',start);depth=1;b=a+1
 while depth:
  depth+=(s[b]=='{')-(s[b]=='}');b+=1
 return 'pub '+s[start:b]
s='''// Pure host adaptation of reference/instructions; regenerate with scripts/extract_admission.py.
use crate::{*, state::*, math};
use primitive_types::U256;
const BPS_DENOMINATOR:u16=10_000;
const MIN_GRADUATION_SOL_THRESHOLD:u64=1_000_000_000;
const MIN_GRADUATION_PRICE_BPS:u16=9500;
const MAX_GRADUATION_PRICE_BPS:u16=10500;
const MAX_UNSOLD_AT_GRADUATION_BPS:u16=100;
macro_rules! require {($condition:expr, $($error:tt)*)=>{ensure($condition,stringify!($($error)*))?};}
macro_rules! error {($($error:tt)*)=>{AmmError::from(stringify!($($error)*))};}
fn math_error(e:math::MathError)->AmmError {AmmError::from(format!("{e:?}"))}
'''
for file,name in [('graduation_safety.rs','graduation_trigger_met'),('graduation_safety.rs','validate_graduation_trigger'),('graduation_safety.rs','graduation_pool_tokens'),('graduation_safety.rs','validate_graduation_economics'),('graduation_policy.rs','legacy_contribution'),('launch_graduation.rs','validate_buy_inventory'),('launch_graduation.rs','validate_continuation')]:
 f=function(file,name)
 if name=='graduation_pool_tokens':f=f.replace('fn graduation_pool_tokens','fn v2_pool_tokens')
 s+=f+'\n'
s+='''fn graduation_pool_tokens(coin:&LaunchedCoin,vault:u64,sol:u64)->Result<u64>{if coin.version>=2 {v2_pool_tokens(coin,vault,sol)}else{legacy_contribution(coin,vault,sol)}}
fn meteora_graduation_rent(rent:&Rent)->Result<u64>{[255,187,165,1112,408,408,465,465,165,165,183,165].into_iter().try_fold(0u64,|sum,size|sum.checked_add(rent.minimum_balance(size)).ok_or_else(||AmmError::from("Rent overflow")))}
'''
s=s.replace('LaunchpadError::from','math_error').replace('super::graduate_meteora::MIN_METEORA_RESERVE','10_000')
import re
s=re.sub(r'\.ok_or\(LaunchpadError::(\w+)\)', r'.ok_or_else(||AmmError::from("\1"))',s)
(r/'src/admission.rs').write_text(s)
