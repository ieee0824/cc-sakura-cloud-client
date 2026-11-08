import { ml_kem768 } from '@noble/post-quantum/ml-kem.js';
// Shared Web Crypto protocol for browsers and interoperability tests.
(function (root) {
  const enc = new TextEncoder();
  function b64(bytes) {
    let text = '';
    for (let i = 0; i < bytes.length; i += 8192) text += String.fromCharCode(...bytes.subarray(i, i + 8192));
    return btoa(text).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');
  }
  function decode(text) {
    if (typeof text !== 'string' || !/^[A-Za-z0-9_-]*$/.test(text)) throw Error('暗号データの形式が不正です。');
    const bytes = Uint8Array.from(atob(text.replaceAll('-', '+').replaceAll('_', '/')), c => c.charCodeAt(0));
    if (b64(bytes) !== text) throw Error('暗号データの形式が不正です。');
    return bytes;
  }
  const id = async raw => b64(new Uint8Array(await crypto.subtle.digest('SHA-256', raw)));
  const suite='ml-kem-768+p256-hkdf-sha256-v2';
  async function deviceId(publicKey) {
    if(publicKey?.suite!==suite || Object.keys(publicKey).sort().join(',')!=='ml_kem,p256,suite') throw Error('対応していない端末鍵です。');
    const point=publicKey.p256;
    if(point?.kty!=='EC'||point.crv!=='P-256'||Object.keys(point).sort().join(',')!=='crv,kty,x,y'||decode(point.x).length!==32||decode(point.y).length!==32||decode(publicKey.ml_kem).length!==1184) throw Error('端末鍵の形式が不正です。');
    await crypto.subtle.importKey('jwk',point,{name:'ECDH',namedCurve:'P-256'},false,[]);
    return id(enc.encode(JSON.stringify([suite,point.x,point.y,publicKey.ml_kem])));
  }
  const contextFor=(server,project,user,device,key)=>enc.encode(JSON.stringify(['cc-sakura-room-key-v2',suite,server,project,user,device,key]));
  async function wrappingKey(ecdh,kem,nonce,context,usage) {
    const shared=new Uint8Array(64);shared.set(new Uint8Array(ecdh));shared.set(kem,32);
    try {
      const material=await crypto.subtle.importKey('raw',shared,'HKDF',false,['deriveKey']);
      return await crypto.subtle.deriveKey({name:'HKDF',hash:'SHA-256',salt:nonce,info:context},material,{name:'AES-GCM',length:256},false,[usage]);
    } finally {shared.fill(0);new Uint8Array(ecdh).fill(0);kem.fill(0);}
  }
  async function validate(bundle, server, project) {
    if (bundle?.v !== 1 || bundle.server_id !== server || bundle.project_id !== project || typeof bundle.key !== 'string') throw Error('復旧キーの対象プロジェクトが違います。');
    const raw = decode(bundle.key);
    if (raw.length !== 32 || await id(raw) !== bundle.key_id) throw Error('復旧キーが不正です。');
    return crypto.subtle.importKey('raw', raw, 'AES-GCM', false, ['encrypt', 'decrypt']);
  }
  function aad(bundle, member, operation) {
    return enc.encode(JSON.stringify(['cc-sakura-e2ee-v1', bundle.server_id, bundle.project_id, member, operation, bundle.key_id]));
  }
  function rootBundle(bundle){return {v:1,server_id:bundle.server_id,project_id:bundle.project_id,key_id:bundle.key_id,key:bundle.key};}
  function historyAAD(bundle,previous){return enc.encode(JSON.stringify(['cc-sakura-key-history-v1',bundle.server_id,bundle.project_id,bundle.key_id,previous]));}
  root.SakuraE2EE = {
    rootBundle,
    async rotate(bundle) {
      await validate(bundle,bundle.server_id,bundle.project_id);
      const next=await this.generate(bundle.server_id,bundle.project_id);
      const nonce=crypto.getRandomValues(new Uint8Array(12));
      const ciphertext=await crypto.subtle.encrypt({name:'AES-GCM',iv:nonce,additionalData:historyAAD(next,bundle.key_id)},await validate(next,next.server_id,next.project_id),enc.encode(JSON.stringify(rootBundle(bundle))));
      return {bundle:{...next,history:[rootBundle(bundle),...(bundle.history??[])]},archive:{key_id:next.key_id,previous_key_id:bundle.key_id,nonce:b64(nonce),ciphertext:b64(new Uint8Array(ciphertext))}};
    },
    async restoreHistory(bundle,records) {
      if(!Array.isArray(records)||records.length>10000)throw Error('履歴用の鍵が多すぎます。');
      await validate(bundle,bundle.server_id,bundle.project_id);
      const links=new Map(records.map(r=>[r.key_id,r]));
      if(links.size!==records.length)throw Error('履歴用の鍵が重複しています。');
      const seen=new Set([bundle.key_id]),history=[];let current=rootBundle(bundle);
      while(links.has(current.key_id)) {
        const link=links.get(current.key_id),nonce=decode(link.nonce);
        if(nonce.length!==12||seen.has(link.previous_key_id))throw Error('履歴用の鍵の連鎖が不正です。');
        const clear=await crypto.subtle.decrypt({name:'AES-GCM',iv:nonce,additionalData:historyAAD(current,link.previous_key_id)},await validate(current,bundle.server_id,bundle.project_id),decode(link.ciphertext));
        const previous=JSON.parse(new TextDecoder('utf-8',{fatal:true}).decode(clear));
        await validate(previous,bundle.server_id,bundle.project_id);
        if(previous.key_id!==link.previous_key_id)throw Error('履歴用の鍵が一致しません。');
        seen.add(previous.key_id);history.push(rootBundle(previous));current=previous;
      }
      return {...rootBundle(bundle),history};
    },
    deviceId,
    async generateDevice() {
      const pair=await crypto.subtle.generateKey({name:'ECDH',namedCurve:'P-256'},false,['deriveBits']);
      const exported=await crypto.subtle.exportKey('jwk',pair.publicKey);
      const kem=ml_kem768.keygen();
      const public_key={suite,p256:{kty:exported.kty,crv:exported.crv,x:exported.x,y:exported.y},ml_kem:b64(kem.publicKey)};
      const fingerprint=await deviceId(public_key);
      const storageKey=await crypto.subtle.generateKey({name:'AES-GCM',length:256},false,['encrypt','decrypt']);
      const storageNonce=crypto.getRandomValues(new Uint8Array(12));
      try {
        const sealedKemKey=await crypto.subtle.encrypt({name:'AES-GCM',iv:storageNonce,additionalData:enc.encode(fingerprint)},storageKey,kem.secretKey);
        return {privateKey:pair.privateKey,public_key,id:fingerprint,storageKey,storageNonce,sealedKemKey};
      } finally {kem.secretKey.fill(0);}
    },
    async wrap(bundle, recipientUser, device) {
      await validate(bundle,bundle.server_id,bundle.project_id);
      if(await deviceId(device.public_key)!==device.id) throw Error('配送先の公開鍵が一致しません。');
      const recipient=await crypto.subtle.importKey('jwk',device.public_key.p256,{name:'ECDH',namedCurve:'P-256'},false,[]);
      const pair=await crypto.subtle.generateKey({name:'ECDH',namedCurve:'P-256'},false,['deriveBits']);
      const nonce=crypto.getRandomValues(new Uint8Array(12));
      const context=contextFor(bundle.server_id,bundle.project_id,recipientUser,device.id,bundle.key_id);
      const shared=await crypto.subtle.deriveBits({name:'ECDH',public:recipient},pair.privateKey,256);
      const kem=ml_kem768.encapsulate(decode(device.public_key.ml_kem));
      const key=await wrappingKey(shared,kem.sharedSecret,nonce,context,'encrypt');
      const ciphertext=await crypto.subtle.encrypt({name:'AES-GCM',iv:nonce,additionalData:context},key,enc.encode(JSON.stringify(rootBundle(bundle))));
      const pub=await crypto.subtle.exportKey('jwk',pair.publicKey);
      return {suite,device_id:device.id,kem_ciphertext:b64(kem.cipherText),ephemeral:{kty:pub.kty,crv:pub.crv,x:pub.x,y:pub.y},nonce:b64(nonce),ciphertext:b64(new Uint8Array(ciphertext))};
    },
    async unwrap(device, envelope, server, project, user, keyId) {
      if(envelope.suite!==suite || decode(envelope.kem_ciphertext).length!==1088) throw Error('対応していない鍵配送方式です。');
      if(await deviceId(device.public_key)!==device.id) throw Error('端末鍵が一致しません。');
      if(envelope.device_id!==device.id) throw Error('別の端末宛ての暗号鍵です。');
      const ephemeral=await crypto.subtle.importKey('jwk',envelope.ephemeral,{name:'ECDH',namedCurve:'P-256'},false,[]);
      const nonce=decode(envelope.nonce);
      if(nonce.length!==12) throw Error('配送された鍵の形式が不正です。');
      const context=contextFor(server,project,user,device.id,keyId);
      const shared=await crypto.subtle.deriveBits({name:'ECDH',public:ephemeral},device.privateKey,256);
      const decapsulationKeyBytes=new Uint8Array(await crypto.subtle.decrypt({name:'AES-GCM',iv:device.storageNonce,additionalData:enc.encode(device.id)},device.storageKey,device.sealedKemKey));
      let kem;
      try {kem=ml_kem768.decapsulate(decode(envelope.kem_ciphertext),decapsulationKeyBytes);} finally {decapsulationKeyBytes.fill(0);}
      const key=await wrappingKey(shared,kem,nonce,context,'decrypt');
      const clear=await crypto.subtle.decrypt({name:'AES-GCM',iv:nonce,additionalData:context},key,decode(envelope.ciphertext));
      const bundle=JSON.parse(new TextDecoder('utf-8',{fatal:true}).decode(clear));
      await validate(bundle,server,project);
      if(bundle.key_id!==keyId) throw Error('roomの暗号鍵が一致しません。');
      return bundle;
    },
    async generate(server, project) {
      const raw = crypto.getRandomValues(new Uint8Array(32));
      return { v: 1, server_id: server, project_id: project, key_id: await id(raw), key: b64(raw) };
    },
    validate,
    async encrypt(bundle, member, operation, payload) {
      const key = await validate(bundle, bundle.server_id, bundle.project_id);
      const nonce = crypto.getRandomValues(new Uint8Array(12));
      const ciphertext = await crypto.subtle.encrypt({ name: 'AES-GCM', iv: nonce, additionalData: aad(bundle, member, operation), tagLength: 128 }, key, enc.encode(JSON.stringify(payload)));
      return { type: 'e2ee-v1', key_id: bundle.key_id, nonce: b64(nonce), ciphertext: b64(new Uint8Array(ciphertext)), operation_id: operation };
    },
    async decrypt(bundle, member, payload) {
      if (payload?.type !== 'e2ee-v1') throw Error('対応する復旧キーが必要です。');
      if(payload.key_id!==bundle.key_id)bundle=bundle.history?.find(key=>key.key_id===payload.key_id);
      if(!bundle)throw Error('対応する履歴の鍵がありません。');
      const key = await validate(bundle, bundle.server_id, bundle.project_id);
      const nonce = decode(payload.nonce);
      if (nonce.length !== 12) throw Error('暗号データの形式が不正です。');
      const clear = await crypto.subtle.decrypt({ name: 'AES-GCM', iv: nonce, additionalData: aad(bundle, member, payload.operation_id), tagLength: 128 }, key, decode(payload.ciphertext));
      return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(clear));
    },
  };
})(globalThis);
