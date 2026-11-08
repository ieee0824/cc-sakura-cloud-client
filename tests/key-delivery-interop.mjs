// Rust interop test helper. Only synthetic test bundles and public keys enter here.
import './browser/e2ee.js';
let input='';for await(const chunk of process.stdin)input+=chunk;
const args=JSON.parse(input);
if(args.mode==='rotation') {
  const first=await SakuraE2EE.generate('server','project');
  const old=await SakuraE2EE.encrypt(first,'member','operation-old',{message:'old history'});
  const second=await SakuraE2EE.rotate(first),third=await SakuraE2EE.rotate(second.bundle);
  const payload=await SakuraE2EE.encrypt(third.bundle,'member','operation-new',{message:'new history'});
  process.stdout.write(JSON.stringify({bundle:SakuraE2EE.rootBundle(third.bundle),first,old,payload,history:[third.archive,second.archive]}));
} else if(args.mode==='decrypt') {
  const clear=await SakuraE2EE.decrypt(args.bundle,args.member,args.payload);
  process.stdout.write(JSON.stringify(clear));
} else {
  const bundle=await SakuraE2EE.generate(args.server,args.project);
  const envelope=await SakuraE2EE.wrap(bundle,`agent:${args.member}`,args.device);
  process.stdout.write(JSON.stringify({bundle,envelope}));
}
