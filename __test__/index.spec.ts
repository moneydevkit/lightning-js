import test from 'ava'

import { MdkNode } from '../index'

test('can generate an invoice', (t) => {
  const mdkNode = new MdkNode({
    network: 'signet',
    esploraUrl: 'https://mutinynet.com/api',
    rgsUrl: 'https://rgs.mutinynet.com/snapshot',
    mnemonic: 'abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about',
    lspNodeId: '02465ed5be53d04fde66c9418ff14a5f2267723810176c9212b722e542dc1afb1b',
    lspAddress: '45.79.52.207:9735',
  })

  const invoice = mdkNode.getInvoice(1000, 'test invoice', 15 * 60)

  t.is(invoice.bolt11.startsWith('lntbs'), true)
})
