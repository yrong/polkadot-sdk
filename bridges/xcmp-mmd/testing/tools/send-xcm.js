#!/usr/bin/env node

// Send a test XCM message from source para 1000 to dest para 2000
// Usage: node send-xcm.js

const { ApiPromise, WsProvider, Keyring } = require('@polkadot/api');

async function main() {
  console.log('Connecting to source parachain (1000) at ws://127.0.0.1:9945...');

  const wsProvider = new WsProvider('ws://127.0.0.1:9945');
  const api = await ApiPromise.create({ provider: wsProvider });

  console.log('Connected! Chain:', (await api.rpc.system.chain()).toString());

  // Create Alice account
  const keyring = new Keyring({ type: 'sr25519' });
  const alice = keyring.addFromUri('//Alice');
  console.log('Sending from Alice:', alice.address);

  // Destination: Para 2000
  const dest = {
    V4: {
      parents: 1,
      interior: {
        X1: [{ Parachain: 2000 }]
      }
    }
  };

  // Simple XCM message: UnpaidExecution + ClearOrigin
  const message = {
    V4: [
      {
        UnpaidExecution: {
          weightLimit: 'Unlimited',
          checkOrigin: null
        }
      },
      {
        ClearOrigin: null
      }
    ]
  };

  console.log('\nSending XCM message:');
  console.log('  Destination: Para 2000');
  console.log('  Message: UnpaidExecution + ClearOrigin');

  // Send the XCM
  const tx = api.tx.polkadotXcm.send(dest, message);

  console.log('\nSubmitting transaction...');

  const unsub = await tx.signAndSend(alice, ({ status, events }) => {
    console.log('Transaction status:', status.type);

    if (status.isInBlock) {
      console.log('✓ Included in block:', status.asInBlock.toHex());

      // Check for events
      events.forEach(({ event }) => {
        const { section, method, data } = event;
        console.log(`  Event: ${section}.${method}`, data.toString());
      });
    } else if (status.isFinalized) {
      console.log('✓ Finalized in block:', status.asFinalized.toHex());
      console.log('\n✅ XCM message sent successfully!');
      console.log('\nNext steps:');
      console.log('1. Check source para headers for xmmd digest');
      console.log('2. Start the relayer to construct proofs');
      console.log('3. Verify message delivery on dest para');

      unsub();
      process.exit(0);
    } else if (status.isError) {
      console.error('❌ Transaction failed');
      unsub();
      process.exit(1);
    }
  });
}

main().catch((error) => {
  console.error('Error:', error);
  process.exit(1);
});
