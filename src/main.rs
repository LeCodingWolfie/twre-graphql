use graphql_parser::query::{parse_query, 
    Definition, OperationDefinition, Selection, Field};
use std::path::{Path, PathBuf};
use serde_json::{Value, Map};
use itertools::Itertools;
use std::error::Error;
use regex::Regex;
use std::fs;
use url::Url;

type BoxError = Result<(), Box<dyn Error>>;

fn escape(string: &str) -> String {
    string.replace("\\'", "'").replace("\\\"", "\"").replace("\"", "\\\"")
}

fn regex_captures_with_key(regex: Regex, key_regex: &Regex, haystack: &str, prefix: &str) -> Vec<String> {
    regex.captures_iter(haystack).map(|captures| captures.iter().map(|x| match x { 
        None => None, Some(capture) => Some(key_regex.replace_all(&capture.as_str()[prefix.len()..capture.len()],
            |caps: &regex::Captures| format!(r#""{}":"#, &caps[0][0..caps[0].len() - 1])).to_string())
    }).flatten().collect::<Vec<_>>()).flatten().collect::<Vec<_>>()
}

fn regex_captures(regex: Regex, haystack: &str, prefix: &str) -> Vec<String> {
    regex.captures_iter(haystack).map(|captures| captures.iter().map(|x| match x { 
        None => None, Some(capture) => Some(capture.as_str().strip_prefix(prefix).unwrap_or_default().to_string())
    }).flatten().collect::<Vec<_>>()).flatten().collect::<Vec<_>>()
}

fn chunkify(slice: &[String]) -> &[String; 2] {
    slice.try_into().expect("Not a two-sized chunk.")
}

fn write_request(url: String, filename: String, directory: &str) -> BoxError {
    let path = format!("{directory}/{filename}");
    
    if !Path::new(&path).exists() {
        let response = ureq::get(url.as_str()).call()?.into_string()?;
        fs::write(path, response)?;
        println!("Downloaded {filename}.");
    } else {
        println!("{filename} was downloaded already.");
    }
    
    Ok(())
}

fn download_assets() -> BoxError {
    const VOD_HTML: &str = "1976811369.html";
    const ASSET_BASE_URL: &str = "https://static.twitchcdn.net/assets";

    let html = fs::read_to_string(VOD_HTML).expect("Unable to read VOD HTML file.");
    let js_regex = Regex::new(r#"="[^"]*\.js"#).expect("Unable to parse .js regex.");
    let assets_regex = Regex::new(r"\+\{[\s\S][^}]*}").expect("Unable to parse assets/ regex.");
    let key_regex = Regex::new(r"[0-9a-z]*:").expect("Unable to parse key regex.");

    for js in regex_captures(js_regex, html.as_str(), "=\"") {
        let url = Url::parse(js.as_str())?;
        let filename = url.path_segments().ok_or_else(|| "Cannot be base.")?.last().unwrap().to_string();
        write_request(js, filename, "js")?;
    }
    
    let assets_captures = regex_captures_with_key(assets_regex, &key_regex, html.as_str(), "+");
    for (index, slice) in assets_captures.chunks(2).collect::<Vec<_>>().iter().enumerate() {
        let extension = if index != 0 { "css" } else { "js" };
        let path = format!("{extension}/assets");     
        let [names_json, alphanumeric_json] = chunkify(slice);
        let names_value: Value = serde_json::from_str(names_json)?;
        let alphanumeric_value: Value = serde_json::from_str(alphanumeric_json)?;
        let filenames = names_value.as_object().unwrap().iter().zip(
            alphanumeric_value.as_object().unwrap().iter()).map(|tuple| {
                let (name, alphanumeric) = (tuple.0.1, tuple.1.1);
                format!("{name}-{alphanumeric}.{extension}").replace("\"", "")
            }).collect::<Vec<_>>();

        if !Path::new(&path).exists() {
            fs::create_dir(&path)?;
        }
    
        for filename in filenames {
            let url = format!("{}/{filename}", ASSET_BASE_URL);
            write_request(url, filename, path.as_str())?;
        }
    }

    Ok(())
}

type Object = Map<String, Value>;
type SingleField<'a> = Field<'a, String>;
type Items<'a> = Vec<Selection<'a, String>>;

fn collect_fields<'a, F>(items: Items<'a>, closure: F) -> Vec<String>// SingleField<'a>>
    where F: Fn(SingleField<'a>) -> String // SingleField<'a>
{
    items.into_iter().map(|item| match item {
        Selection::Field(field) => Some(closure(field)), _ => None
    }).flatten().collect::<Vec<_>>()
}

fn extract_graphql_json(path: PathBuf) -> (Object, Vec<[Vec<String>; 2]>) {
    const JSON_DIRECTORY: &str = "json";
    const JS_EXTENSION: &str = ".js";
    const BODY_PREFIX: &str = r#"y":"#;
    
    let filename = path.file_name().unwrap().to_str().unwrap();
    let basename = &filename[0..filename.len() - JS_EXTENSION.len()];
    let mut name_fragments = basename.split('-').collect::<Vec<_>>(); 
    name_fragments.pop();
    
    let var_characters = match name_fragments.join("-").as_str() {
        "core" => "[ti]",
        _ => "n"
    };

    let body_regex_string = format!(r"{}=\{{k[\s\S][^{{]*[\s\S][^;]*", var_characters);
    let source_regex_string = format!(r"{}.loc.source=\{{[\s\S][^;]*", var_characters);
        
    let javascript = fs::read_to_string(&path).expect("Unable to read chat-video.js file.");
    let body_regex = Regex::new(body_regex_string.as_str()).expect("Unable to parse regex.");
    let source_regex = Regex::new(source_regex_string.as_str()).expect("Unable to parse `n.loc.source` regex.");
    let source_body_regex = Regex::new(r#"y":['"][\s\S][^,]*"#).expect("Unable to parse `source.body` regex.");
    let key_regex = Regex::new(r"[A-Za-z]*:").expect("Unable to parse key regex.");

    let base_captures = regex_captures_with_key(body_regex, &key_regex, javascript.as_str(), "n=");
    let source_captures = regex_captures_with_key(source_regex, &key_regex, javascript.as_str(), "n.loc.source=");
    
    let captures = base_captures.into_iter().zip(source_captures).collect::<Vec<_>>();
    
    let graphql_name_regex = Regex::new(r#""[A-Za-z]*":"#).expect("Unable to parse GraphQL name regex.");   
    
    let mut graphql_definitions = Vec::new();
    let mut json = Map::new();
    
    for tuple in captures {
        let (mut body, mut source) = tuple;
        body = body.replace("!1", "true").replace("!0", "false");
        let source_body = source_body_regex.captures(source.as_str()).unwrap().iter()
            .map(|m| { let capture = m.unwrap().as_str(); &capture[BODY_PREFIX.len()..capture.len()] })
            .next().expect("`source.body` should have been captured, but has not.");

        let source_body_unescape = source_body.replace("\\\"n", "\n\"").replace("\\n", "\n");
        let source_body_unescape = graphql_name_regex.replace_all(&source_body_unescape,
            |caps: &regex::Captures| format!(r#"{}:"#, &caps[0][1..caps[0].len() - 2]));

        let source_body_escape = format!(r#""{}""#, escape(&source_body[1..source_body.len() - 1]).as_str());
        source = source.replace(source_body, source_body_escape.as_str());

        let mut body_value: Value = serde_json::from_str(body.as_str()).expect("Unable to parse body JSON");
        let source_body_value: Value = serde_json::from_str(source.as_str()).expect("Unable to parse `n.loc.source` JSON");
        body_value["loc"]["source"] = source_body_value;
        
        let definitions = body_value["definitions"].as_array().unwrap();
        let name = definitions[0]["name"]["value"].as_str().unwrap();
        let kind = definitions[0]["kind"].as_str().unwrap();        

        json.insert(name.to_string(), body_value.clone());

        let graphql_query = parse_query::<String>(&source_body_unescape[1..source_body_unescape.len() - 1]).unwrap();
                                                
        graphql_definitions.append(
            &mut graphql_query.definitions.into_iter().map(|definition| match definition {
                Definition::Operation(operation) => match operation {
                    OperationDefinition::Query(query) => Some((query.variable_definitions, query.selection_set.items)),
                    OperationDefinition::Mutation(mutation) => Some((mutation.variable_definitions, mutation.selection_set.items)), _ => None
                }, Definition::Fragment(_) => None
            }).flatten().map(|array| {
                let (variable_definitions, items) = array;
                let variables = variable_definitions.into_iter().map(|variable| variable.var_type.to_string()).collect::<Vec<_>>();
                let items = collect_fields(items, |root| root.name.to_string());
                [variables, items]
            }).collect::<Vec<_>>());
        
        if definitions.len() > 1 {
            println!("{name} ({}) is of length {}!", kind, definitions.len());
        }
    }
     
    if json.len() == 0 {
        println!("Object has no key-value pairs.");
    }

    (json, graphql_definitions)
}

fn main() -> BoxError {
    download_assets()?;
    
    let mut paths = fs::read_dir("js")?.chain(fs::read_dir("js\\assets")?)
        .collect::<Vec<_>>(); _ = paths.remove(0);

    let mut map: Map<String, Value> = Map::new();
    let mut definitions = Vec::new();

    for path in paths {
        let path = path?.path();
        println!("{}", path.display());
        let (mut json, mut graphql_definitions) = extract_graphql_json(path);
        definitions.append(&mut graphql_definitions);
        map.append(&mut json);
    }

    println!("{} key-value pairs in the GraphQL definitions.", definitions.len());
    
    println!("{} key-value pairs in the map.", map.len());
        
    let json = serde_json::to_string_pretty(&map).expect("Unable to stringify JSON");
    fs::write("graphql.json", json)?;

    let [types, items] = definitions.into_iter().reduce(|mut accumulator, mut array| {
        for (index, mut element) in accumulator.clone().into_iter().enumerate() {
            element.append(&mut array[index]); accumulator[index] = element;
        }; accumulator
    }).unwrap();
 
    let types = types.into_iter().map(|mut string| {
        string.retain(|character| !r#"![]"#.contains(character)); string
    }).unique().collect_vec();

    let items = items.into_iter().unique().collect_vec();
    
    println!("{:?} {:?} {}", types, items, items.len());

    Ok(())
}