mod garble;
mod playfair;
mod sap;
mod sap_msg_tables;
mod sap_tables;
#[cfg(test)]
mod sap_vectors;
mod tables;

pub use sap::FairPlayClient;

use crate::{Error, Result};

const REPLY_HEADER: [u8; 12] = [
    0x46, 0x50, 0x4c, 0x59, 0x03, 0x01, 0x04, 0x00, 0x00, 0x00, 0x00, 0x14,
];

pub struct FairPlayServer {
    keymsg: [u8; 164],
    keymsg_len: usize,
}

impl Default for FairPlayServer {
    fn default() -> Self {
        Self {
            keymsg: [0; 164],
            keymsg_len: 0,
        }
    }
}

impl FairPlayServer {
    // Phase 1 (16-byte request) replies with 142 bytes; phase 2 (164-byte request) with 32 bytes.
    // `response` is cleared and replaced with the reply body.
    pub fn setup(&mut self, request: &[u8], response: &mut Vec<u8>) -> Result<()> {
        if request.len() != 16 && request.len() != 164 {
            return Err(Error::protocol("longitud de fp-setup no válida"));
        }
        if request[4] != 0x03 {
            return Err(Error::protocol("versión de FairPlay no soportada"));
        }
        response.clear();
        if request.len() == 16 {
            let mode = usize::from(request[14]);
            let reply = tables::REPLY_MESSAGES
                .get(mode)
                .ok_or_else(|| Error::protocol("modo de FairPlay no soportado"))?;
            response.extend_from_slice(reply);
            self.keymsg_len = 0;
        } else {
            self.keymsg.copy_from_slice(request);
            self.keymsg_len = 164;
            response.extend_from_slice(&REPLY_HEADER);
            response.extend_from_slice(&request[144..164]);
        }
        Ok(())
    }

    pub fn decrypt_key(&self, ekey: &[u8; 72]) -> Result<[u8; 16]> {
        if self.keymsg_len != 164 {
            return Err(Error::Crypto("fp-setup incompleto".into()));
        }
        if self.keymsg[12] > 3 {
            return Err(Error::Crypto("modo de FairPlay no soportado".into()));
        }
        Ok(playfair::playfair_decrypt(&self.keymsg, ekey))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    fn fnv1a(data: &[u8]) -> u64 {
        data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    // Generated with UxPlay's C code (fairplay_playfair.c + lib/playfair) built with MSVC.
    const REPLY_HEAD: [&str; 4] = [
        "46504c59030102000000008202000f9f",
        "46504c5903010200000000820201cf32",
        "46504c5903010200000000820202c169",
        "46504c59030102000000008202039001",
    ];
    const REPLY_FNV: [u64; 4] = [
        0x3d8e4e02084b2887,
        0xc7ba827810db9176,
        0xe5b4faf447ecd35f,
        0xf6928fb6c9c9cd28,
    ];
    // (keymsg, ekey, expected key), keymsg[12] cycles through the four modes.
    const DECRYPT_VECTORS: [(&str, &str, &str); 8] = [
        (
            "0b02e536034ed61ab049b85600d63ffc7d556bc86d9a9c82edc1cd6969986c34787fd0c073e7260c564894e02f9ac4ed41a81ee038492a4efd2a10306d86a02e633279d2fa78fb9c9e87ffc2adb31833d935355b2dc8f4718623a771b764ce50bb85371ff65aebe97b77c3bb76f5390653f3ded05e6f214442be06d8e66cbeb3fe07b842b791fd4823bdee93b036dd38e73284772d83e535335575ac537be75f419b15c5",
            "351810ebaab376fada0fed9ba9db677896bf3b09ade8ba41673983d991e133ca7d9ebb12f2d3b043c096d79782d437ff01e935130d2f64040996a61c81202a8d43dd555726a69eb7",
            "fcbb12946540ca9add3bba0d8c04dd03",
        ),
        (
            "a70b7df5038991b18a9e66e701fe734606c31cdad8ce0f9e16a040097f47f80bf361b666ee0b7be5aaa9cdbc35ffd452500c1c5c8ca3276e5ffcd8e849d2ca3ae73b93707726a99b092319e49b7779c84bd37e00bb1362794c5774abcf137c0d522d52213ebf6638464e078c8dce75069b423daf88adc3be9c3c3abd43a03729d1b959a9c5506a4104ee85b933bf6533957d6054930acb609ca8980aeadfa1f2eec83665",
            "587ba8550e731be6e9bd3c57eb953d74cbd9a8570358b90acf065b15c8dedb852764c1dc851907eaf8cad3535632a655cb9cda48e9d93ce3a905fe088e029bd80e0c4020ed4a0366",
            "9e05db02ad95242356b2df1b7f88d561",
        ),
        (
            "7f3f6dd303a335bcdac75cb902dbf9ade23e138e717bb11cd7e272105ddc1273ea6906a1e7b3cca69b0ed4c7828e6a4826b5005d27df3ee4de98b3c035f98e1d393d5c9421e9306a2d1ae0832f92986ba8e51ca08f1a89d8162429a8b33e810c171604ebac4d6d5b9630d230dc7167c422e61a05eb6692c2274ebdf2bfdae85f12bfabf08b5309191a7e42d7a8c0a80a990f47cf5972ea09d5df6727fa5db5f0f25abb38",
            "68ecd85cd1fd7a1cc08a6772e7d089549de1290a4287499352587acb218e46a1a266bd02fcd7d8eb8c46db01bb856419bf0bffb110bda8fe026c3969976a86d03a622f41f3338bd1",
            "6f54971628bd1bf5b6322899f9c73131",
        ),
        (
            "04269d6f03147b2bc6fdabf0037fcf81d2556ef33ffb00579e804ba09556cd669382b5d3cce99dcf64e738c0dff331345ac1c47608ac2953fe73b43526962ab49777e2c1a101af699f684301884f1fb72c84a8087eb494c80aaa049050fd2ccb6fcb427ee42032e7dba54f2c20ae5f0fdf440c9556f632ec194f83de1b9f6052764e916475c71b4f53f0de7967646e1a654a01b96da641c71a87a5d901570982da48fb2f",
            "c1c1e9f825ba9f2713f33e11f0ba367f1e0ada722fe16b7171e695995b9486bc34fa5c2586ab7dbe31a869f59fc341fa538f8e4cdadad73868bd045d370c5573cf470a26ef1acb8f",
            "0605802d564b7d068d3a1b3296b8a662",
        ),
        (
            "9413233403a102ae311938be004f315632db60d50bcf1cb6a1de931c6d503842cd0a485eafe5fefd03a5c575065f7e14b1bf8f349c59b49f3304cb6107d316580161d4a1651115010139d56c931c88d768f0c0e703f0b02292bab39caba068a92c075736ba185f21d48467d97ffa393bf02b7efde1f7f7c7caf7154bfc07f929ddb65e318d1934f1f0cb547a69160b119e2acb4ee57b8af84d6ad37e4240d07e6d5323ea",
            "0894a78eb614815059287547fd1ca279762ad518132a1445f75f580340448a8c6b74bbdd37049151a164816f60a90d51c08656b373e69642192fd7e1c2d3091f30415d509b3b8b86",
            "5d435e17caa7f28824207cd5b519bba0",
        ),
        (
            "fe98ffd4036c324bd109ca3101923651db97a1a456066e37a57e58223a0aa1a166400a5705d724fe400fa681d2d2727593493980c03185ca8f041c5eeab343f74f0121024bcf87eb501b7a13966c1d0fbcd291049b71ee9ae8cc2ad0557a43c4d6790d6549a07a203d3dbf45c5c3537401f5b0dd232d7d24f627103aa52ad910c753f1aa5bdefbb47f6184a024864e65401a7ffe4733181fde8a2b6a48498750e9a5a9ed",
            "545572fe6ea3bea5f9355208078ee91b10a7f2bcf0d355c3ad7e504978523ceaa30e2104b51f54ed02718bf06909a651e17004d40e2d6b2ce1e559727f8f9747ee3492e686a434a0",
            "9e4df4067b3dc01669a0d458d0a89475",
        ),
        (
            "1178a83503c2399d950bf4f60266bf43a603e96e786e6b8fec43c8750ddce06c5a85e0527d7f1a1d51b9200300422d9b47cac056bf249de8f9e34e13d77c9aea66a945bff69af5c47068204dbcf6997dc703a6022a2305ea53745a107dcc3a2afb2862350bc3fd3efcf6a950f776072b427cea9bddf189a207abac7e22d0a3576e7557ed10e0e0d9137e3e6d752303dd574904a35e54b9ec25268da8e4709bbb38f8cf0e",
            "7f551fddf0a080cb15cbe6f1ab706a67c249fa80596dfa7569f83dff11ef54f4dd41186f9e5828b3a1004ccd49fb078cd4241fef5a1ab55f6b0ebd008c2fcbda3d5bff6bc47b1897",
            "ba58d9f6f958147c68e24ac8738aa94e",
        ),
        (
            "69c9fe610384f7d53b9e988d038b602c6b555160a6336022236347512d5dd9ff7399df28710b977e6c305a7e69fa2a772265c1c8f69c317a2658da936bd77bbb2c161485e9e47598e82739c7226f76c28b22f19665d8e828a0fb60af241bc4e54c7c8c1b1cecc470fd07e29f68cd2df7d2a49f9d59b01093a2b344c7b32c0269de40767b64a2eec121925160ad512263d6c0cc15557974a87a0024fb699c69b48fc20317",
            "f5e2575058768e006f50b9dba4ee294c3639aad73fc185b1d536b70ca7008061b9edd367a91f4a0c76719d2b8c8af83d68022f0808ace0d458049b21daaee12036e61d051189cd1c",
            "f0937a92fdc26991c9c497cab74852ac",
        ),
    ];

    #[test]
    fn phase1_replies_match_reference_for_each_mode() {
        for mode in 0..4u8 {
            let mut request = [0u8; 16];
            request[..4].copy_from_slice(b"FPLY");
            request[4] = 3;
            request[14] = mode;
            let mut server = FairPlayServer::default();
            let mut response = vec![1, 2, 3];
            server.setup(&request, &mut response).unwrap();
            assert_eq!(response.len(), 142);
            assert_eq!(response[..16], unhex(REPLY_HEAD[usize::from(mode)])[..]);
            assert_eq!(fnv1a(&response), REPLY_FNV[usize::from(mode)]);
        }
    }

    #[test]
    fn phase1_rejects_bad_requests() {
        let mut server = FairPlayServer::default();
        let mut response = Vec::new();
        let mut request = [0u8; 16];
        request[4] = 2;
        assert!(server.setup(&request, &mut response).is_err());
        request[4] = 3;
        request[14] = 4;
        assert!(server.setup(&request, &mut response).is_err());
        assert!(server.setup(&[0u8; 10], &mut response).is_err());
    }

    #[test]
    fn phase2_reply_is_header_plus_last_20_bytes() {
        let mut request = [0u8; 164];
        for (i, b) in request.iter_mut().enumerate() {
            *b = i as u8;
        }
        request[4] = 3;
        let mut server = FairPlayServer::default();
        let mut response = Vec::new();
        server.setup(&request, &mut response).unwrap();
        assert_eq!(response.len(), 32);
        assert_eq!(response[..12], REPLY_HEADER);
        assert_eq!(response[12..], request[144..]);
    }

    #[test]
    fn decrypt_key_requires_phase2() {
        let server = FairPlayServer::default();
        assert!(server.decrypt_key(&[0; 72]).is_err());
    }

    #[test]
    fn decrypt_key_matches_reference_implementation() {
        for (i, (keymsg, ekey, expected)) in DECRYPT_VECTORS.iter().enumerate() {
            let keymsg = unhex(keymsg);
            assert_eq!(usize::from(keymsg[12]), i % 4);
            let ekey: [u8; 72] = unhex(ekey).try_into().unwrap();
            let mut server = FairPlayServer::default();
            let mut response = Vec::new();
            server.setup(&keymsg, &mut response).unwrap();
            assert_eq!(
                server.decrypt_key(&ekey).unwrap().to_vec(),
                unhex(expected),
                "vector {i}"
            );
        }
    }

    #[test]
    fn client_completes_handshake_with_server_and_key_round_trips() {
        for mode in 0..4u8 {
            let mut client = FairPlayClient::new();
            let mut server = FairPlayServer::default();
            let mut m1 = client.message1();
            m1[14] = mode;
            let mut m2 = Vec::new();
            server.setup(&m1, &mut m2).unwrap();
            let m3 = client.exchange(&m2).unwrap();
            let mut m4 = Vec::new();
            server.setup(&m3, &mut m4).unwrap();
            client.confirm(&m4).unwrap();

            let mut raw = [0u8; 16];
            rand::fill(&mut raw[..]);
            let ekey = client.wrap_key(&raw).unwrap();
            assert_eq!(server.decrypt_key(&ekey).unwrap(), raw, "modo {mode}");
        }
    }
}
