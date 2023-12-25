use nom::{
    number::complete::{be_u16, be_u32, be_u8},
    IResult,
};

pub(crate) fn parse_name(input: &[u8]) -> IResult<&[u8], String> {
    let mut res = String::new();
    let mut i = input;
    loop {
        match be_u8(i)? {
            (remaining, 0) => {
                // End of the name
                return Ok((remaining, res));
            }
            (remaining, length) => {
                let label_end = length as usize;
                let label = &remaining[0..label_end];
                let label_str = String::from_utf8_lossy(label);
                if !res.is_empty() {
                    res.push('.');
                }
                res.push_str(&label_str);
                i = &remaining[label_end..];
            }
        }
    }
}

pub(crate) fn parse_rr(input: &[u8]) -> IResult<&[u8], ResourceRecord> {
    let (input, name) = parse_name(input)?;
    let (input, type_) = be_u16(input)?;
    let (input, class) = be_u16(input)?;
    let (input, ttl) = be_u32(input)?;
    let (input, rd_length) = be_u16(input)?;
    let (input, rdata) = nom::bytes::complete::take(rd_length)(input)?;

    Ok((
        input,
        ResourceRecord {
            name,
            type_,
            class,
            ttl,
            rdata: rdata.to_vec(),
        },
    ))
}

#[derive(Debug)]
pub(crate) struct ResourceRecord {
    pub name: String,
    pub type_: u16,
    pub class: u16,
    #[allow(unused)]
    pub ttl: u32,
    pub rdata: Vec<u8>,
}
