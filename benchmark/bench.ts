import { Bench } from 'tinybench'

import { MdkNode } from '../index.js'

const b = new Bench()

b.add('Generate an invoice', () => {
  const mdkNode = new MdkNode({
    network: 'signet',
    esploraUrl: 'https://mutinynet.com/api',
    rgsUrl: 'https://rgs.mutinynet.com/snapshot',
    mnemonic: 'abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about',
    lspNodeId: '02465ed5be53d04fde66c9418ff14a5f2267723810176c9212b722e542dc1afb1b',
    lspAddress: '45.79.52.207:9735',
  })

  mdkNode.getInvoice(1000, 'test invoice', 15 * 60)
})

await b.run()

console.table(b.table())
